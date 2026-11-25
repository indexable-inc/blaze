//! High-throughput, zero-bounds-check LZ77 block match finders (`lz77_scan_fast` and
//! `lz77_scan_default`), Huffman literal encoders, and reverse FSE sequence packers backing
//! `bstd::zstd`.
//!
//! - `lz77_scan_fast`: 64 KiB 10-bit-tagged 7-byte prime hash table (`LZ77_FAST_HASH_LOG = 14`,
//!   half of M5 Max L1D size matching C `zstd -1`, 1 MiB window, 22-bit position + 1-bit non-zero
//!   + 9-bit tag), 7-byte minMatch on hash hits (4-byte on repeats), zero-spill sequential
//!   2-position miss loop with in-register 2-instruction `tag_matches`, in-loop `rep[0]` check at
//!   both `pos + 1` and `pos1 + 1` and `step_acc += 1` (`>> 6`, stride 128) recurrence, backward
//!   match extension, in-match `match_start + 2` (`ml >= 8`) & `pos - 2` inserts, and zero-literal
//!   `rep[1]` + 1-literal `rep[0]` + 2-literal `rep[1]` chain harvesting in a zero-call leaf
//!   kernel.
//! - `lz77_scan_default`: 10-bit-tagged Double-Fast (`64K` `hash8` long table = 256 KiB +
//!   `32K` `hash5` short table = 128 KiB L1D-resident, 2 MiB window), zero-spill sequential
//!   2-position miss loop with in-register `tag_matches` and `step_acc += 3` (`>> 6`) recurrence,
//!   hit-only lazy upgrades at `hit_pos + 1` (`ht_long` for `m_ml < 14` on long hits and
//!   `m_ml < 48` on short hits, `ht_short` for `m_ml <= 7`, `rep[0]` at `hit_pos + 1`), backward
//!   match extension, in-match table inserts (`ht_short` at `match_start + 1`, both tables at
//!   `match_start + 2` for `m_ml >= 5` and `pos - 2`), and zero-literal `rep[1]` + 1-literal
//!   `rep[0]` chain harvesting in a zero-call leaf kernel.

use core::alloc::Allocator;
use core::ptr;

use crate::{AllocFailure, Int, SeqOp, Vec};

/// C `zstd` `prime8bytes` / `xxHash64` `PRIME64_1` (`0x9E37_79B1_85EB_CA87`).
const HASH8_PRIME: u64 = 11_400_714_785_074_694_791;
const MASK_5BYTES: u64 = 0x0000_00FF_FFFF_FFFF;
const MASK_7BYTES: u64 = 0x00FF_FFFF_FFFF_FFFF;

/// Packed 10-bit-tagged hash entry layout (`u32`):
/// - bits `0..22`: `pos & WIN_MASK_U32` (covers up to 4 MiB, strictly larger than 1 MiB & 2 MiB
///   windows)
/// - bit `22`: `TAG_NONZERO` (`1 << 22`, always set on populated entries so `0` never matches)
/// - bits `23..32`: 9-bit hash tag (`h[39..48]`, disjoint from `log <= 16` bucket index bits)
const WIN_BITS: u32 = 22;
const MAX_WIN_MASK: usize = (1 << WIN_BITS) - 1;
const WIN_MASK_U32: u32 = (1u32 << WIN_BITS) - 1;
const TAG_NONZERO: u32 = 1u32 << WIN_BITS;

/// Hash table size for [`lz77_scan_fast`]: 32,768 `u32` entries = 128 KiB (matches Apple Silicon's
/// 128 KiB L1D cache, halving hash evictions vs 16K at zero extra instruction cost).
pub const LZ77_FAST_HASH_LOG: u32 = 15;
/// Number of `u32` entries in [`lz77_scan_fast`]'s hash table.
pub const LZ77_FAST_HASH_SIZE: usize = 1 << LZ77_FAST_HASH_LOG;

/// Double-Fast long table (`hash8`, 1-way `u32` entries): 65,536 entries = 256 KiB.
const DFAST_LONG_LOG: u32 = 16;
const DFAST_LONG_U32S: usize = 1 << DFAST_LONG_LOG;
/// Double-Fast short table (`hash5`, 1-way `u32` entries): 65,536 entries = 256 KiB (fills the
/// exact 512 KiB `LZ77_DEF_BUCKETS` buffer alongside `ht_long`, halving short-table collisions).
const DFAST_SHORT_LOG: u32 = 16;

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
    (pos.truncate_to::<u32>() & WIN_MASK_U32) | TAG_NONZERO
}

/// 2-instruction AArch64 tag check (`eor; cmp w, #0x400, lsl #12`): returns `true` iff all 10 top
/// bits (`TAG_NONZERO` + 9-bit hash tag) of `entry` equal `new_entry`. An empty `0` slot always
/// returns `false` because bit 22 (`TAG_NONZERO`) is always set in `new_entry`.
#[inline(always)]
fn tag_matches(entry: u32, new_entry: u32) -> bool {
    (entry ^ new_entry) < TAG_NONZERO
}

#[inline(always)]
fn offset_from_entry(pos: usize, entry: u32) -> usize {
    pos.wrapping_sub((entry & WIN_MASK_U32).widen_to::<usize>()) & MAX_WIN_MASK
}

/// Single-comparison offset validation equivalent to `off != 0 && off <= win && off <= pos`
/// (exact whenever `pos < usize::MAX`, which holds since `pos <= end <= isize::MAX`).
#[inline(always)]
fn off_in_bounds(off: usize, win: usize, pos: usize) -> bool {
    off.wrapping_sub(1) < win.min(pos)
}

/// Single-multiply 5-byte tagged hash returning `(bucket_idx, tagged_entry)`.
///
/// For `log <= 16`, bucket index uses bits `(64 - log)..64` and the 9-bit tag uses disjoint bits
/// `39..48`, with bit 22 set to `TAG_NONZERO`.
#[inline(always)]
fn hash5_tagged(val: u64, pos: usize, log: u32) -> (usize, u32) {
    let h = (val & MASK_5BYTES).wrapping_mul(HASH8_PRIME);
    let idx = (h >> (64u32 - log)).truncate_to::<usize>();
    let tag = (h >> 39).truncate_to::<u32>() << 23;
    (idx, pos_low_nz(pos) | tag)
}

/// Single-multiply 7-byte tagged hash returning `(bucket_idx, tagged_entry)`.
///
/// For `log <= 16`, bucket index uses bits `(64 - log)..64` and the 9-bit tag uses disjoint bits
/// `39..48`, with bit 22 set to `TAG_NONZERO`.
#[inline(always)]
fn hash7_tagged(val: u64, pos: usize, log: u32) -> (usize, u32) {
    let h = (val & MASK_7BYTES).wrapping_mul(HASH8_PRIME);
    let idx = (h >> (64u32 - log)).truncate_to::<usize>();
    let tag = (h >> 39).truncate_to::<u32>() << 23;
    (idx, pos_low_nz(pos) | tag)
}

/// Single-multiply 8-byte tagged hash returning `(bucket_idx, tagged_entry)`.
///
/// For `log <= 16`, bucket index uses bits `(64 - log)..64` and the 9-bit tag uses disjoint bits
/// `39..48`, with bit 22 set to `TAG_NONZERO`.
#[inline(always)]
fn hash8_tagged(val: u64, pos: usize, log: u32) -> (usize, u32) {
    let h = val.wrapping_mul(HASH8_PRIME);
    let idx = (h >> (64u32 - log)).truncate_to::<usize>();
    let tag = (h >> 39).truncate_to::<u32>() << 23;
    (idx, pos_low_nz(pos) | tag)
}

#[inline(always)]
unsafe fn match_len_ptr(
    mut a: *const u8,
    mut b: *const u8,
    b_end8: *const u8,
    b_end: *const u8,
) -> usize {
    let b_start = b;
    // SAFETY: Caller guarantees `a <= b <= b_end` within the same input slice and
    // `b_end8 == b_end.sub(8)` (which holds whenever `'scan` enters since `end >= 34`).
    // Whenever `b <= b_end8`, both `a..a + 8` and `b..b + 8` are readable within `base..b_end`,
    // and `a.add(8)` / `b.add(8)` stay within `base..=b_end`.
    unsafe {
        if b <= b_end8 {
            let d0 = ptr::read_unaligned(a.cast::<u64>()) ^ ptr::read_unaligned(b.cast::<u64>());
            if d0 != 0 {
                return (d0.to_le().trailing_zeros() >> 3).widen_to::<usize>();
            }
            loop {
                a = a.add(8);
                b = b.add(8);
                if b > b_end8 {
                    break;
                }
                let diff =
                    ptr::read_unaligned(a.cast::<u64>()) ^ ptr::read_unaligned(b.cast::<u64>());
                if diff != 0 {
                    let tz = (diff.to_le().trailing_zeros() >> 3).widen_to::<usize>();
                    return b.offset_from_unsigned(b_start) + tz;
                }
            }
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
    // SAFETY: `pos <= limit = dict.len() - 8` gives 8 readable bytes at `base.add(pos)`;
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
    // `131_072 <= 131_072` `u32`s. `pos <= limit = dict.len() - 8` gives 8 readable bytes, and
    // `hl`/`hs` are bounded by their respective table logs.
    unsafe {
        let ht_short = ht_long.add(DFAST_LONG_U32S);
        let mut pos = 0usize;
        while pos <= limit {
            let v = read_u64_unaligned(base.add(pos));
            let (hl, el) = hash8_tagged(v, pos, DFAST_LONG_LOG);
            let (hs, es) = hash5_tagged(v, pos, DFAST_SHORT_LOG);
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
    let mut pos = start;
    let mut lit_anchor = start;
    let mut step_acc: usize = 1 << 6;
    let match_limit = end.saturating_sub(33);
    let [mut r0, mut r1, mut r2] = *rep;

    // SAFETY:
    // - Caller guarantees `base..base + end` is readable (`start <= end <= isize::MAX`),
    //   `ht` points to `LZ77_FAST_HASH_SIZE` `u32`s, `lit_start` has `(end - start) + 48`
    //   writable bytes non-overlapping with `base`, and `seq_start` has `(end - start) / 3 + 32`
    //   writable `SeqOp` slots.
    // - Loop runs only while `pos < pos1 = pos + (step_acc >> 6) <= match_limit = end - 33`
    //   (never when `end < 34`), giving >= 33 readable bytes at `base.add(pos)` and
    //   `base.add(pos1)` and >= 32 readable bytes at `pos1 + 1 <= end - 32` (sufficient for
    //   8-byte unaligned loads, `match_len_ptr` from `+ 8` with `b_end8 == b_end.sub(8)`, and
    //   paired 32-byte wildcopy slack
    //   `lit_anchor + lit_len + 31 = hit_pos + 31 <= end - 1 < end`).
    // - Safe-API garbage-`ht` soundness: `prev_entry` loaded from `ht` may contain arbitrary bits
    //   if a safe caller passed an unzeroed `hash_table`. Before dereferencing `base.add(prev_pos)`,
    //   the hit handler checks `off_in_bounds(off, win, hit_pos)`, guaranteeing
    //   `0 <= prev_pos = hit_pos - off < hit_pos <= end - 33` and `1 <= off <= win`.
    // - `step_acc` starts at `64`, resets to `64` on match emit, grows by 14 per `2 * step` bytes
    //   on misses (quadratic distance growth -> `O(sqrt(end))` total), and grows by 1 per >= 1
    //   byte on false tag hits, so `pos + (step_acc >> 6)` cannot overflow `usize`.
    // - `hash7_tagged` indices are top `LZ77_FAST_HASH_LOG` bits, so `h0, h1 < LZ77_FAST_HASH_SIZE`.
    // - Whenever `can_check_r0` is true (`1 <= r0 <= win && r0 <= initial_pos + 1`), `pos` only
    //   increases inside the miss loop with `pos + 1 <= pos1 <= match_limit <= end - 33`, so
    //   `pos + 1` satisfies `0 <= (pos + 1) - r0 < pos + 1 <= end - 33`, giving >= 34 readable
    //   bytes at `base.add(pos + 1 - r0)` and >= 33 readable bytes at `base.add(pos + 1)`.
    // - In the repeat harvester, `pos <= match_limit = end - 33` with `off_in_bounds(r1, win, pos)`,
    //   keeping all reads strictly within `base..base + end`; the zero-literal `rep[1]` branch
    //   writes 0 literals and advances `pos` by `rml >= 4`.
    // - `lit_dst` writes at most `end - start` committed bytes (+ <= 31B paired wildcopy slack)
    //   within `lit_start`'s `(end - start) + 48` reserved bytes.
    // - Every emitted sequence consumes `match_len >= 4` input bytes, so at most `(end - start) / 4`
    //   `SeqOp`s are written within `seq_start`'s `(end - start) / 3 + 32` reserved slots.
    unsafe {
        let b_end = base.add(end);
        let b_end8 = base.add(end.saturating_sub(8));
        'scan: loop {
            let mut step = step_acc >> 6;
            let mut pos1 = pos + step;
            if pos1 > match_limit {
                break;
            }
            let can_check_r0 = off_in_bounds(r0, win, pos + 1);
            let (mut hit_pos, cur8, prev_entry) = loop {
                let c0 = read_u64_unaligned(base.add(pos));
                if can_check_r0
                    && (c0 >> 8).truncate_to::<u32>() == read_u32_unaligned(base.add(pos + 1 - r0))
                {
                    break (pos + 1, 0u64, 0u32);
                }
                let (h0, e0) = hash7_tagged(c0, pos, LZ77_FAST_HASH_LOG);
                let p0 = *ht.add(h0);
                *ht.add(h0) = e0;
                if tag_matches(p0, e0) {
                    break (pos, c0, p0);
                }

                let c1 = read_u64_unaligned(base.add(pos1));
                let (h1, e1) = hash7_tagged(c1, pos1, LZ77_FAST_HASH_LOG);
                let p1 = *ht.add(h1);
                *ht.add(h1) = e1;
                if tag_matches(p1, e1) {
                    break (pos1, c1, p1);
                }

                pos = pos1 + step;
                step_acc += 14;
                step = step_acc >> 6;
                pos1 = pos + step;
                if pos1 > match_limit {
                    break 'scan;
                }
            };

            let (off, mut prev_pos, diff) = if prev_entry == 0 {
                // Direct `rep[0]` match found inside the miss loop at `hit_pos = pos + 1`
                // (`hit_pos > lit_anchor`, `1 <= r0 <= hit_pos`, `r0 <= win`, first 4 bytes
                // already verified equal).
                let pp = hit_pos - r0;
                let d =
                    read_u64_unaligned(base.add(hit_pos)) ^ read_u64_unaligned(base.add(pp));
                (r0, pp, d)
            } else {
                let off = offset_from_entry(hit_pos, prev_entry);
                if !off_in_bounds(off, win, hit_pos) {
                    pos = hit_pos + 1;
                    step_acc += 1;
                    continue;
                }
                let pp = hit_pos - off;
                let d = cur8 ^ read_u64_unaligned(base.add(pp));
                if (d & MASK_7BYTES) != 0 {
                    pos = hit_pos + 1;
                    step_acc += 1;
                    continue;
                }
                (off, pp, d)
            };

            let mut ml = if diff != 0 {
                (diff.trailing_zeros() >> 3).widen_to::<usize>()
            } else {
                8 + match_len_ptr(base.add(prev_pos + 8), base.add(hit_pos + 8), b_end8, b_end)
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
                let tail_pos = pos - 2;
                let tail8 = read_u64_unaligned(base.add(tail_pos));
                let (ht_idx, ht_e) = hash7_tagged(tail8, tail_pos, LZ77_FAST_HASH_LOG);
                *ht.add(ht_idx) = ht_e;
            }

            // Harvest zero-literal `rep[1]` ping-pong chains at `pos` (matching C `zstd_fast.c`;
            // 1-literal `rep[0]` at `pos + 1` is already checked on the first iteration of `'scan`).
            while pos <= match_limit && off_in_bounds(r1, win, pos) {
                let rprev0 = pos - r1;
                let cur_p0_8 = read_u64_unaligned(base.add(pos));
                let d0 = read_u64_unaligned(base.add(rprev0)) ^ cur_p0_8;
                if d0.truncate_to::<u32>() != 0 {
                    break;
                }
                let t = r0;
                r0 = r1;
                r1 = t;
                let rml = if d0 != 0 {
                    (d0.trailing_zeros() >> 3).widen_to::<usize>()
                } else {
                    8 + match_len_ptr(base.add(rprev0 + 8), base.add(pos + 8), b_end8, b_end)
                };
                ptr::write(
                    seq_dst,
                    SeqOp {
                        lit_len: 0,
                        match_len: rml.truncate_to::<u32>(),
                        offset: 1,
                    },
                );
                seq_dst = seq_dst.add(1);
                pos += rml;
                lit_anchor = pos;
                if pos <= match_limit && pos >= 2 {
                    let tail_pos = pos - 2;
                    let tail8 = read_u64_unaligned(base.add(tail_pos));
                    let (ht_idx, ht_e) = hash7_tagged(tail8, tail_pos, LZ77_FAST_HASH_LOG);
                    *ht.add(ht_idx) = ht_e;
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

/// Fast 10-bit-tagged 7-byte-hash LZ77 block scanner (64 KiB `16K` table, 1 MiB window, 7-byte
/// minMatch on hash hits and 4-byte on repeats, zero-spill sequential 2-position miss loop with
/// in-register 2-instruction `tag_matches`, in-loop `rep[0]` check at `pos + 1` & `pos1 + 1` and
/// `step_acc += 1` / `>> 6` stride-128 recurrence, backward match extension, `match_start + 2`
/// (`ml >= 8`) & `pos - 2` inserts, zero-literal `rep[1]` + 1-literal `rep[0]` + 2-literal
/// `rep[1]` chain harvesting, zero inner-loop bounds checks).
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
    let win = max_window.min(1 << 20).max(1);

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
        let (hs, es) = hash5_tagged(v, pos, DFAST_SHORT_LOG);
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
    // SAFETY: Caller guarantees `pos + 8 <= input.len()`, `ht_long` has
    // `1 << DFAST_LONG_LOG` entries, and `ht_short` has `1 << DFAST_SHORT_LOG` entries.
    unsafe {
        let v = read_u64_unaligned(base.add(pos));
        let (hl, el) = hash8_tagged(v, pos, DFAST_LONG_LOG);
        let (hs, es) = hash5_tagged(v, pos, DFAST_SHORT_LOG);
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
    let mut pos = start;
    let mut lit_anchor = start;
    let mut step_acc: usize = 1 << 6;
    let match_limit = end.saturating_sub(33);
    let [mut r0, mut r1, mut r2] = *rep;

    // SAFETY:
    // - Caller guarantees `base..base + end` is readable (`start <= end <= isize::MAX`),
    //   `ht_long` points to `DFAST_LONG_U32S + (1 << DFAST_SHORT_LOG) = 131_072` `u32`s within
    //   `hash_table`'s `131_072` `u32` buffer, `lit_start` has `(end - start) + 48` writable bytes
    //   non-overlapping with `base`, and `seq_start` has `(end - start) / 3 + 32` writable `SeqOp`
    //   slots.
    // - Loop runs only while `pos < pos1 = pos + (step_acc >> 6) <= match_limit = end - 33`
    //   (never when `end < 34`), giving >= 33 readable bytes at `base.add(pos)` and
    //   `base.add(pos1)` (and thus at `hit_pos`) and >= 32 readable bytes at
    //   `p1 = hit_pos + 1 <= end - 32` (sufficient for 8-byte unaligned loads, `match_len_ptr`
    //   from `+ 8` with `b_end8 == b_end.sub(8)`, and paired 32-byte wildcopy slack
    //   `lit_anchor + lit_len + 31 = m_pos + 31 <= end - 1 < end`).
    // - Safe-API garbage-`ht` soundness: tagged entries loaded from `ht_long`/`ht_short` may
    //   contain arbitrary bits if a safe caller passed an unzeroed `hash_table`. Before
    //   dereferencing `base.add(cand)`, every hit and lazy probe checks
    //   `off_in_bounds(off, win, target_pos)`, guaranteeing
    //   `0 <= target_pos - off < target_pos <= end - 32` and `1 <= off <= win`.
    // - `step_acc` starts at `64`, resets to `64` on match emit, grows by 16 per `2 * step` bytes
    //   on misses (quadratic distance growth -> `O(sqrt(end))` total), and grows by 1 per >= 1
    //   byte on a false tag hit, so `pos + (step_acc >> 6)` cannot overflow `usize` (since
    //   `end <= isize::MAX`).
    // - `hash8_tagged`/`hash5_tagged` indices are top `DFAST_LONG_LOG`/`DFAST_SHORT_LOG` bits, so
    //   `hl0, hl1 < 1 << DFAST_LONG_LOG` and `hs0, hs1 < 1 << DFAST_SHORT_LOG`.
    // - Whenever `rep[0]` is checked at `p1 = hit_pos + 1 <= end - 32`, `off_in_bounds(r0, win, p1)`
    //   holds, so `0 <= p1 - r0 < p1` has >= 33 readable bytes and `p1` has >= 32 readable bytes.
    // - Every `insert_dfast_*` call checks its target position `p <= match_limit = end - 33`,
    //   giving >= 33 readable bytes for `read_u64_unaligned`.
    // - In the repeat harvester, `rp <= match_limit <= end - 33` and `off_in_bounds(r, win, rp)`,
    //   keeping all reads strictly within `base..base + end`.
    // - `lit_dst` writes at most `end - start` committed bytes (+ <= 31B paired wildcopy slack)
    //   within `lit_start`'s `(end - start) + 48` reserved bytes; `seq_dst` matches
    //   `lz77_scan_fast_leaf`.
    unsafe {
        let ht_short = ht_long.add(DFAST_LONG_U32S);
        let b_end = base.add(end);
        let b_end8 = base.add(end.saturating_sub(8));
        'scan: loop {
            let mut step = step_acc >> 6;
            let mut pos1 = pos + step;
            if pos1 > match_limit {
                break;
            }
            let (hit_pos, cur8, prev_entry, is_long) = loop {
                // Position 0:
                let c0 = read_u64_unaligned(base.add(pos));
                let (hl0, el0) = hash8_tagged(c0, pos, DFAST_LONG_LOG);
                let (hs0, es0) = hash5_tagged(c0, pos, DFAST_SHORT_LOG);
                let pl0 = *ht_long.add(hl0);
                let ps0 = *ht_short.add(hs0);
                *ht_long.add(hl0) = el0;
                *ht_short.add(hs0) = es0;
                if tag_matches(pl0, el0) {
                    break (pos, c0, pl0, true);
                }
                if tag_matches(ps0, es0) {
                    break (pos, c0, ps0, false);
                }

                // Position 1 (`c0, hl0, el0, hs0, es0, pl0, ps0` are all dead -> zero spills):
                let c1 = read_u64_unaligned(base.add(pos1));
                let (hl1, el1) = hash8_tagged(c1, pos1, DFAST_LONG_LOG);
                let (hs1, es1) = hash5_tagged(c1, pos1, DFAST_SHORT_LOG);
                let pl1 = *ht_long.add(hl1);
                let ps1 = *ht_short.add(hs1);
                *ht_long.add(hl1) = el1;
                *ht_short.add(hs1) = es1;
                if tag_matches(pl1, el1) {
                    break (pos1, c1, pl1, true);
                }
                if tag_matches(ps1, es1) {
                    break (pos1, c1, ps1, false);
                }

                pos = pos1 + step;
                step_acc += 16;
                step = step_acc >> 6;
                pos1 = pos + step;
                if pos1 > match_limit {
                    break 'scan;
                }
            };

            let mut m_pos = hit_pos;
            let mut m_prev;
            let mut m_off;
            let mut m_ml;
            let mut inserted_short_p1 = false;

            if is_long {
                // 1. Long (`hash8`) hit at `hit_pos`: reject stale/out-of-window/tag-collision
                //    candidates before computing match length.
                let off = offset_from_entry(hit_pos, prev_entry);
                if !off_in_bounds(off, win, hit_pos) {
                    pos = hit_pos + 1;
                    step_acc += 1;
                    continue 'scan;
                }
                let cand = hit_pos - off;
                let diff = cur8 ^ read_u64_unaligned(base.add(cand));
                if diff.truncate_to::<u32>() != 0 {
                    pos = hit_pos + 1;
                    step_acc += 1;
                    continue 'scan;
                }
                m_prev = cand;
                m_off = off;
                m_ml = if diff != 0 {
                    (diff.trailing_zeros() >> 3).widen_to::<usize>()
                } else {
                    8 + match_len_ptr(base.add(cand + 8), base.add(hit_pos + 8), b_end8, b_end)
                };
                if m_ml < 12 {
                    let p1 = hit_pos + 1;
                    let next8 = read_u64_unaligned(base.add(p1));
                    let (hl1, el1) = hash8_tagged(next8, p1, DFAST_LONG_LOG);
                    let pl1 = *ht_long.add(hl1);
                    *ht_long.add(hl1) = el1;
                    if tag_matches(pl1, el1) {
                        let off1 = offset_from_entry(p1, pl1);
                        if off_in_bounds(off1, win, p1) {
                            let cl1 = p1 - off1;
                            let diff1 = next8 ^ read_u64_unaligned(base.add(cl1));
                            if diff1.truncate_to::<u32>() == 0 {
                                let ml1 = if diff1 != 0 {
                                    (diff1.trailing_zeros() >> 3).widen_to::<usize>()
                                } else {
                                    8 + match_len_ptr(
                                        base.add(cl1 + 8),
                                        base.add(p1 + 8),
                                        b_end8,
                                        b_end,
                                    )
                                };
                                if ml1 > m_ml + 1 || (ml1 > m_ml && off1 < (m_off >> 2)) {
                                    m_pos = p1;
                                    m_prev = cl1;
                                    m_off = off1;
                                    m_ml = ml1;
                                }
                            }
                        }
                    }
                }
            } else {
                // 2. Short (`hash5`, minMatch = 5) hit at `hit_pos` + hit-only lazy upgrades
                //    (`ht_long` at `hit_pos + 1` when `m_ml < 32`, `rep[0]` at `hit_pos + 1`,
                //    and `ht_short` at `hit_pos + 1` when `m_ml <= 7`).
                let off = offset_from_entry(hit_pos, prev_entry);
                if !off_in_bounds(off, win, hit_pos) {
                    pos = hit_pos + 1;
                    step_acc += 1;
                    continue 'scan;
                }
                let cand = hit_pos - off;
                let diff = cur8 ^ read_u64_unaligned(base.add(cand));
                if (diff & MASK_5BYTES) != 0 {
                    pos = hit_pos + 1;
                    step_acc += 1;
                    continue 'scan;
                }
                m_prev = cand;
                m_off = off;
                m_ml = if diff != 0 {
                    (diff.trailing_zeros() >> 3).widen_to::<usize>()
                } else {
                    8 + match_len_ptr(base.add(cand + 8), base.add(hit_pos + 8), b_end8, b_end)
                };
                if m_ml < 32 {
                    let p1 = hit_pos + 1;
                    let next8 = read_u64_unaligned(base.add(p1));
                    let (hl1, el1) = hash8_tagged(next8, p1, DFAST_LONG_LOG);
                    let pl1 = *ht_long.add(hl1);
                    *ht_long.add(hl1) = el1;

                    let mut upgraded = false;
                    if tag_matches(pl1, el1) {
                        let off1 = offset_from_entry(p1, pl1);
                        if off_in_bounds(off1, win, p1) {
                            let cl1 = p1 - off1;
                            let diff1 = next8 ^ read_u64_unaligned(base.add(cl1));
                            if diff1.truncate_to::<u32>() == 0 {
                                let ml1 = if diff1 != 0 {
                                    (diff1.trailing_zeros() >> 3).widen_to::<usize>()
                                } else {
                                    8 + match_len_ptr(
                                        base.add(cl1 + 8),
                                        base.add(p1 + 8),
                                        b_end8,
                                        b_end,
                                    )
                                };
                                if ml1 > m_ml + 1 || (ml1 > m_ml && off1 < (m_off >> 2)) {
                                    m_pos = p1;
                                    m_prev = cl1;
                                    m_off = off1;
                                    m_ml = ml1;
                                    upgraded = true;
                                }
                            }
                        }
                    }

                    if !upgraded
                        && m_ml <= 9
                        && off_in_bounds(r0, win, p1)
                        && (m_off != r0 || m_pos == lit_anchor)
                        && m_off != r1
                        && m_off != r2
                    {
                        let rprev = p1 - r0;
                        let rdiff = next8 ^ read_u64_unaligned(base.add(rprev));
                        if rdiff.truncate_to::<u32>() == 0 {
                            let rml = if rdiff != 0 {
                                (rdiff.trailing_zeros() >> 3).widen_to::<usize>()
                            } else {
                                8 + match_len_ptr(
                                    base.add(rprev + 8),
                                    base.add(p1 + 8),
                                    b_end8,
                                    b_end,
                                )
                            };
                            if rml + 1 >= m_ml {
                                m_pos = p1;
                                m_prev = rprev;
                                m_off = r0;
                                m_ml = rml;
                                upgraded = true;
                            }
                        }
                    }

                    if !upgraded && m_ml <= 7 {
                        let (hs1, es1) = hash5_tagged(next8, p1, DFAST_SHORT_LOG);
                        let ps1 = *ht_short.add(hs1);
                        *ht_short.add(hs1) = es1;
                        inserted_short_p1 = true;
                        if tag_matches(ps1, es1) {
                            let off1 = offset_from_entry(p1, ps1);
                            if off_in_bounds(off1, win, p1) {
                                let cs1 = p1 - off1;
                                let diff1 = next8 ^ read_u64_unaligned(base.add(cs1));
                                if (diff1 & MASK_5BYTES) == 0 {
                                    let ml1 = if diff1 != 0 {
                                        (diff1.trailing_zeros() >> 3).widen_to::<usize>()
                                    } else {
                                        8 + match_len_ptr(
                                            base.add(cs1 + 8),
                                            base.add(p1 + 8),
                                            b_end8,
                                            b_end,
                                        )
                                    };
                                    if ml1 >= m_ml + 2 || (ml1 == m_ml + 1 && off1 < (m_off >> 2)) {
                                        m_pos = p1;
                                        m_prev = cs1;
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
            // tables at `match_start + 2` for `m_ml >= 5` and `pos - 2`).
            if (!inserted_short_p1 || match_start != hit_pos) && match_start + 1 <= match_limit {
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
                if pos > match_limit {
                    break;
                }
                let mut rp = 0usize;
                let mut rprev = 0usize;
                let mut rdiff = 0u64;
                let mut r_lits = 0usize;
                let mut r_ov = 0u32;

                let cur_pos8 = read_u64_unaligned(base.add(pos));
                if off_in_bounds(r1, win, pos) {
                    let rprev0 = pos - r1;
                    let d0 = read_u64_unaligned(base.add(rprev0)) ^ cur_pos8;
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
                if r_ov == 0 && pos + 1 <= match_limit && off_in_bounds(r0, win, pos + 1) {
                    let rp1 = pos + 1;
                    let rprev1 = rp1 - r0;
                    let rprev1_8 = read_u64_unaligned(base.add(rprev1));
                    if (rprev1_8 ^ (cur_pos8 >> 8)).truncate_to::<u32>() == 0 {
                        rp = rp1;
                        rprev = rprev1;
                        rdiff = rprev1_8 ^ read_u64_unaligned(base.add(rp1));
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
                    8 + match_len_ptr(base.add(rprev + 8), base.add(rp + 8), b_end8, b_end)
                };
                if r_lits == 1 {
                    *lit_dst = cur_pos8.truncate_to::<u8>();
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

/// Default 10-bit-tagged Double-Fast (`64K` `hash8` long table = 256 KiB + `32K` `hash5`
/// short table = 128 KiB L1D-resident, 2 MiB window) LZ77 block scanner: zero-spill sequential
/// 2-position miss loop with in-register `tag_matches` and `step_acc += 3` (`>> 6`), hit-only
/// lazy upgrades (`ht_long` for `m_ml < 14` on long hits and `m_ml < 48` on short hits, `rep[0]`
/// at `hit_pos + 1`, `ht_short` for `m_ml <= 7`), backward extension, in-match table inserts
/// (`ht_short` at `match_start + 1`, both tables at `match_start + 2` for `m_ml >= 5` and
/// `pos - 2`), zero-literal `rep[1]` + 1-literal `rep[0]` chain harvesting, zero inner-loop
/// bounds checks.
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
    //   covers `1 << DFAST_SHORT_LOG = 32_768` `u32`s (`98_304 <= 131_072`).
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

/// Zero-call leaf kernel for [`huff_encode_stream_raw`].
///
/// Keeping `try_reserve` and `set_len` in the outer wrapper lets LLVM allocate the hot 4-symbol
/// reverse bitstream loop across all AArch64 caller-saved and callee-saved GPRs with zero stack
/// spills, using raw pointer decrement (`src_ptr = src_ptr.sub(4)`).
#[inline(never)]
unsafe fn huff_encode_stream_leaf(
    src_base: *const u8,
    len: usize,
    codes_ptr: *const u32,
    out_start: *mut u8,
) -> usize {
    let mut out_ptr = out_start;
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    let rem = len & 3;
    let body_len = len - rem;

    // SAFETY:
    // - Caller guarantees `src_base..src_base + len` is readable, `codes_ptr` points to 256 `u32`
    //   entries (`code | (num_bits << 16)` with `num_bits <= 11`), and `out_start` has at least
    //   `len + (len >> 1) + 24` writable bytes non-overlapping with `src_base`.
    // - `tail_ptr` walks backwards from `src_base + len` to `body_end = src_base + body_len` in
    //   1-byte steps; `src_ptr` walks backwards from `body_end` to `src_base` in 4-byte steps
    //   (`body_len` is an exact multiple of 4).
    // - After every unconditional flush `bits <= 7`. Four symbols emit at most `4 * 11 = 44` bits,
    //   so `bits + n32 + n10 <= 7 + 44 = 51 <= 63` (strictly `< 64`) and every 8-byte unaligned
    //   flush stays within reserved capacity.
    unsafe {
        let body_end = src_base.add(body_len);
        let mut tail_ptr = src_base.add(len);
        while tail_ptr != body_end {
            tail_ptr = tail_ptr.sub(1);
            let e = *codes_ptr.add(usize::from(*tail_ptr));
            let code = u64::from(e & 0xFFFF);
            let nb = e >> 16;
            buf |= code << bits;
            bits += nb;
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
            buf >>= bits & !7u32;
            bits &= 7;
        }

        let mut src_ptr = body_end;
        while src_ptr != src_base {
            src_ptr = src_ptr.sub(4);
            let w = read_u32_unaligned(src_ptr);
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

        out_ptr.offset_from_unsigned(out_start)
    }
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
    // SAFETY:
    // - `segment` is readable for `segment.len()` bytes; `packed_codes` has 256 entries.
    // - `dst` has `max_bytes >= segment.len() * 11 / 8 + 24` spare bytes past `start_len`,
    //   disjoint from `segment`, and `huff_encode_stream_leaf` writes `written <= max_bytes - 8`
    //   bytes starting at `dst.as_mut_ptr().add(start_len)`.
    unsafe {
        let out_start = dst.as_mut_ptr().add(start_len);
        let written = huff_encode_stream_leaf(
            segment.as_ptr(),
            segment.len(),
            packed_codes.as_ptr(),
            out_start,
        );
        dst.set_len(start_len + written);
    }

    Ok(())
}

/// Zero-call leaf kernel for [`huff_encode_stream_checked_raw`].
///
/// Returns `None` if any symbol in `src_base..src_base + len` has `num_bits == 0` in `codes_ptr`,
/// otherwise returns the number of encoded bytes written starting at `out_start`.
#[inline(never)]
unsafe fn huff_encode_stream_checked_leaf(
    src_base: *const u8,
    len: usize,
    codes_ptr: *const u32,
    out_start: *mut u8,
) -> Option<usize> {
    let mut out_ptr = out_start;
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    let rem = len & 3;
    let body_len = len - rem;

    // SAFETY: Same buffer, table, and capacity invariants as `huff_encode_stream_leaf`.
    unsafe {
        let body_end = src_base.add(body_len);
        let mut tail_ptr = src_base.add(len);
        while tail_ptr != body_end {
            tail_ptr = tail_ptr.sub(1);
            let e = *codes_ptr.add(usize::from(*tail_ptr));
            let nb = e >> 16;
            if nb == 0 {
                return None;
            }
            let code = u64::from(e & 0xFFFF);
            buf |= code << bits;
            bits += nb;
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
            buf >>= bits & !7u32;
            bits &= 7;
        }

        let mut src_ptr = body_end;
        while src_ptr != src_base {
            src_ptr = src_ptr.sub(4);
            let w = read_u32_unaligned(src_ptr);
            let e3 = *codes_ptr.add((w >> 24).widen_to::<usize>());
            let e2 = *codes_ptr.add(((w >> 16) & 0xFF).widen_to::<usize>());
            let e1 = *codes_ptr.add(((w >> 8) & 0xFF).widen_to::<usize>());
            let e0 = *codes_ptr.add((w & 0xFF).widen_to::<usize>());

            let n3 = e3 >> 16;
            let n2 = e2 >> 16;
            let n1 = e1 >> 16;
            let n0 = e0 >> 16;
            if (n0 == 0) | (n1 == 0) | (n2 == 0) | (n3 == 0) {
                return None;
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

        Some(out_ptr.offset_from_unsigned(out_start))
    }
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
    // SAFETY: Same buffer, table, and capacity invariants as `huff_encode_stream_raw`. On `None`
    // return, `dst.set_len` is not called so `dst.len()` remains `start_len`.
    unsafe {
        let out_start = dst.as_mut_ptr().add(start_len);
        let Some(written) = huff_encode_stream_checked_leaf(
            segment.as_ptr(),
            segment.len(),
            packed_codes.as_ptr(),
            out_start,
        ) else {
            return Ok(false);
        };
        dst.set_len(start_len + written);
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

const _: () = {
    assert!(core::mem::offset_of!(FseSeqEncTables, ll_tt) == 0);
    assert!(core::mem::offset_of!(FseSeqEncTables, of_tt) == 64 * 8);
    assert!(core::mem::offset_of!(FseSeqEncTables, ml_tt) == 128 * 8);
    assert!(core::mem::offset_of!(FseSeqEncTables, ll_st) == 768 * 2);
    assert!(core::mem::offset_of!(FseSeqEncTables, of_st) == 1024 * 2);
    assert!(core::mem::offset_of!(FseSeqEncTables, ml_st) == 1280 * 2);
};

#[inline(always)]
unsafe fn fse_step_indexed<const TT_OFF: usize, const ST_OFF: usize>(
    tables: &FseSeqEncTables,
    sym: u8,
    state: u32,
) -> (u32, u32, u32) {
    let base = ptr::from_ref(tables);
    // SAFETY: Compile-time `offset_of!` assertions above verify that `TT_OFF` (`0`, `64`, `128`)
    // is the exact `u64`-element offset of `ll_tt`/`of_tt`/`ml_tt` (64 entries each, disjoint from
    // `sym & 63`) and `ST_OFF` (`768`, `1024`, `1280`) is the exact `u16`-element offset of
    // `ll_st`/`of_st`/`ml_st` (256 entries each, disjoint from `idx & 255`) within
    // `#[repr(C)] FseSeqEncTables` (`align >= 8`). `nb <= 9 < 32`.
    unsafe {
        let tt_idx = (usize::from(sym) & 63) | TT_OFF;
        let packed = *base.cast::<u64>().add(tt_idx);
        let delta_nb_bits = packed.truncate_to::<u32>();
        let delta_find_state = (packed >> 32).truncate_to::<u32>();
        let nb = state.wrapping_add(delta_nb_bits) >> 16;
        let bits = state & !u32::MAX.wrapping_shl(nb);
        let st_idx =
            ((state >> nb).wrapping_add(delta_find_state) & 255).widen_to::<usize>() | ST_OFF;
        let next = u32::from(*base.cast::<u16>().add(st_idx));
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

/// Zero-call leaf kernel for [`fse_pack_sequences_fast`].
///
/// Keeping `try_reserve` and `set_len` in the outer wrapper makes this a zero-call leaf function
/// (unlocking all 27 AArch64 GPRs); indexing all six FSE tables off one `noalias`
/// `&FseSeqEncTables` base pointer and staging `OF -> ML -> LL` emission keeps peak live values
/// well below 27 GPRs so zero table pointers spill to the stack. Walks `raw_seqs` via raw pointer
/// decrement (`sp = sp.sub(1)` -> 1-instruction `sub x, x, #12`), uses
/// `state & !u32::MAX.wrapping_shl(nb)` (`lsl` + `bic`), and performs two unconditional branchless
/// 64-bit flushes per sequence:
/// - Chunk 1 (`of_st_bits [<=8] + ml_st_bits [<=9] + ll_st_bits [<=9] + ll_extra [<=16]`):
///   `7 + 8 + 9 + 9 + 16 = 49 <= 63` bits before Flush #1 (leaving `bits <= 7`).
/// - Chunk 2 (`ml_extra [<=16] + of_extra [<=31]`):
///   `7 + 16 + 31 = 54 <= 63` bits before Flush #2 (leaving `bits <= 7`).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn fse_pack_sequences_leaf(
    seq_start: *const SeqOp,
    n: usize,
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
    out_start: *mut u8,
) -> usize {
    let mut ll_state = init_ll;
    let mut of_state = init_of;
    let mut ml_state = init_ml;

    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    let mut out_ptr = out_start;
    let ll_lut_ptr = ll_lut.as_ptr();
    let ml_lut_ptr = ml_lut.as_ptr();

    // SAFETY:
    // - Caller guarantees `n >= 1`, `seq_start..seq_start + n` is readable, `ll_lut`, `ml_lut`,
    //   and `tables` are valid immutable references, and `out_start` has `n * 12 + 48` writable
    //   bytes disjoint from inputs (each sequence emits <= 89 bits < 12 bytes, leaving >= 48 bytes
    //   of slack for every 8-byte unaligned flush).
    // - `sp` starts at `seq_start.add(n - 1)` and decrements by 1 until `sp == seq_start`,
    //   visiting `seq_start[n - 2]` down to `seq_start[0]` strictly in-bounds.
    // - Both unconditional flushes maintain `bits <= 7` at entry and stay `<= 54 < 64` before
    //   each flush (`bits & !7 <= 48 < 64`).
    unsafe {
        // Emit last sequence's extra bits (`last_ll_bits <= 16`, `last_ml_bits <= 16`,
        // `last_of_bits <= 31`).
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

        let mut sp = seq_start.add(n - 1);
        while sp != seq_start {
            sp = sp.sub(1);
            let off = (*sp).offset;
            let of_b = 31u32 - off.max(1).leading_zeros();
            let of_extra = off & !u32::MAX.wrapping_shl(of_b);
            let (next_of, of_st_bits, of_st_nb) =
                fse_step_indexed::<64, 1024>(tables, of_b.truncate_to::<u8>(), of_state);
            of_state = next_of;

            let ml_len = (*sp).match_len;
            let (ml_code, ml_extra, ml_b) = encode_ml_lut_raw(ml_len, ml_lut_ptr);
            let (next_ml, ml_st_bits, ml_st_nb) =
                fse_step_indexed::<128, 1280>(tables, ml_code, ml_state);
            ml_state = next_ml;
            let p_om = u64::from(of_st_bits) | (u64::from(ml_st_bits) << of_st_nb);
            let n_om = of_st_nb + ml_st_nb;

            let ll_len = (*sp).lit_len;
            let (ll_code, ll_extra, ll_b) = encode_ll_lut_raw(ll_len, ll_lut_ptr);
            let (next_ll, ll_st_bits, ll_st_nb) =
                fse_step_indexed::<0, 768>(tables, ll_code, ll_state);
            ll_state = next_ll;

            // Chunk 1: `of_st_bits (<=8) + ml_st_bits (<=9) + ll_st_bits (<=9) + ll_extra (<=16)`
            // + residual `bits (<=7)` <= 49 <= 63 bits -> unconditional branchless Flush #1.
            let p_ll = u64::from(ll_st_bits) | (u64::from(ll_extra) << ll_st_nb);
            let n_ll = ll_st_nb + ll_b;
            buf |= p_om << bits;
            buf |= p_ll << (bits + n_om);
            bits += n_om + n_ll;
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
            buf >>= bits & !7u32;
            bits &= 7;

            // Chunk 2: `ml_extra (<=16) + of_extra (<=31)` + residual `bits (<=7)` <= 54 <= 63 bits
            // -> unconditional branchless Flush #2.
            let p_mo = u64::from(ml_extra) | (u64::from(of_extra) << ml_b);
            buf |= p_mo << bits;
            bits += ml_b + of_b;
            ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
            out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
            buf >>= bits & !7u32;
            bits &= 7;
        }

        let ml_final = ml_state.wrapping_sub(u32::from(tables.ml_table_size));
        let of_final = of_state.wrapping_sub(u32::from(tables.of_table_size));
        let ll_final = ll_state.wrapping_sub(u32::from(tables.ll_table_size));
        buf |= u64::from(ml_final) << bits;
        bits += u32::from(tables.ml_log);
        buf |= u64::from(of_final) << bits;
        bits += u32::from(tables.of_log);
        buf |= u64::from(ll_final) << bits;
        bits += u32::from(tables.ll_log);
        ptr::write_unaligned(out_ptr.cast::<u64>(), buf.to_le());
        out_ptr = out_ptr.add((bits >> 3).widen_to::<usize>());
        buf >>= bits & !7u32;
        bits &= 7;

        buf |= 1u64 << bits;
        *out_ptr = buf.truncate_to::<u8>();
        out_ptr = out_ptr.add(1);

        out_ptr.offset_from_unsigned(out_start)
    }
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
    let max_bytes = n.saturating_mul(12).saturating_add(48);
    comp_buf.try_reserve(max_bytes)?;

    let start_len = comp_buf.len();
    // SAFETY:
    // - `n >= 1`, `raw_seqs`, `ll_lut`, `ml_lut`, and `tables` are valid immutable references.
    // - `comp_buf` has `n * 12 + 48` spare bytes past `start_len`, disjoint from inputs, and
    //   `fse_pack_sequences_leaf` writes `written <= max_bytes - 8` bytes starting at `out_start`.
    unsafe {
        let out_start = comp_buf.as_mut_ptr().add(start_len);
        let written = fse_pack_sequences_leaf(
            raw_seqs.as_ptr(),
            n,
            ll_lut,
            ml_lut,
            tables,
            init_ll,
            init_of,
            init_ml,
            last_ll_extra,
            last_ll_bits,
            last_ml_extra,
            last_ml_bits,
            last_of_extra,
            last_of_bits,
            out_start,
        );
        comp_buf.set_len(start_len + written);
    }

    Ok(())
}

/// Zero-call leaf sequence symbol histogrammer directly over `&[SeqOp]`.
///
/// Walks `raw_seqs` with raw pointer increment (`p = p.add(1)`) and indexes `ll_lut`, `ml_lut`,
/// and the three fixed-size count arrays directly without `Slice::iter` `Option` checks or
/// slice bounds checks.
#[inline(never)]
pub fn count_seq_symbols_fast(
    raw_seqs: &[SeqOp],
    ll_lut: &[u32; 64],
    ml_lut: &[u32; 128],
    ll_counts: &mut [u32; 64],
    of_counts: &mut [u32; 32],
    ml_counts: &mut [u32; 64],
) {
    let ll_lut_ptr = ll_lut.as_ptr();
    let ml_lut_ptr = ml_lut.as_ptr();
    let ll_cnt = ll_counts.as_mut_ptr();
    let of_cnt = of_counts.as_mut_ptr();
    let ml_cnt = ml_counts.as_mut_ptr();

    // SAFETY:
    // - `p` starts at `raw_seqs.as_ptr()` and advances by 1 until `p == end = raw_seqs.as_ptr().add(raw_seqs.len())`,
    //   so every `*p` field read stays strictly within `raw_seqs`.
    // - `ll_lut_ptr` points to 64 `u32`s and is dereferenced only when `ll < 64`; `ml_lut_ptr`
    //   points to 128 `u32`s and is dereferenced only when `adj < 128`.
    // - `ll_cnt` (64 `u32`s), `of_cnt` (32 `u32`s), and `ml_cnt` (64 `u32`s) come from disjoint
    //   `&mut` array references and are indexed with `& 63`, `& 31`, and `& 63`, guaranteeing
    //   in-bounds exclusive access for arbitrary `SeqOp` and LUT inputs.
    unsafe {
        let mut p = raw_seqs.as_ptr();
        let end = p.add(raw_seqs.len());
        while p < end {
            let ll = (*p).lit_len;
            let ll_c = if ll < 64 {
                (*ll_lut_ptr.add(ll.widen_to::<usize>()) & 0xFF).widen_to::<usize>()
            } else {
                50u32.wrapping_sub(ll.leading_zeros()).min(35).widen_to::<usize>()
            };

            let ml = (*p).match_len;
            let adj = ml.wrapping_sub(3);
            let ml_c = if adj < 128 {
                (*ml_lut_ptr.add(adj.widen_to::<usize>()) & 0xFF).widen_to::<usize>()
            } else if ml < 3 {
                0
            } else {
                67u32.wrapping_sub(adj.leading_zeros()).min(52).widen_to::<usize>()
            };

            let of = (*p).offset;
            let of_c = (31u32 - (of | 1).leading_zeros()).widen_to::<usize>();

            let ll_slot = ll_cnt.add(ll_c & 63);
            *ll_slot = (*ll_slot).wrapping_add(1);
            let of_slot = of_cnt.add(of_c & 31);
            *of_slot = (*of_slot).wrapping_add(1);
            let ml_slot = ml_cnt.add(ml_c & 63);
            *ml_slot = (*ml_slot).wrapping_add(1);

            p = p.add(1);
        }
    }
}
