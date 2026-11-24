//! Zero-pre-zeroing byte output cursor over a [`Vec<u8, A>`]'s initialized prefix and
//! uninitialized spare capacity.
//!
//! Designed for high-throughput compression/decompression (`bstd::zstd`, `bstd::lz4`,
//! `bstd::snappy`) and serializers: callers get safe, register-resident writes, 16-byte wild
//! literal copies, and LZ77 overlapping match copies directly into spare capacity without ever
//! paying a pre-zeroing pass (`memset(0)`) over the output buffer.

use core::alloc::Allocator;
use core::marker::PhantomData;
use core::ptr;
use core::slice;

use crate::{AllocFailure, Vec};

/// A decoded LZ77 sequence `(lit_len, match_len, offset)` for batched execution via
/// [`ByteCursor::exec_seq_batch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqOp {
    pub lit_len: u32,
    pub match_len: u32,
    pub offset: u32,
}

impl SeqOp {
    /// Zero-initialized placeholder entry for fixed-size stack batches.
    pub const ZERO: Self = Self { lit_len: 0, match_len: 0, offset: 0 };
}

/// A register-cached, zero-pre-zeroing append cursor over a [`Vec<u8, A>`].
///
/// Maintains `0..self.len` as initialized bytes within `self.base` (`self.len <= self.cap <=
/// self.vec.capacity()`) and `self.len..self.cap` as valid allocated spare capacity belonging to
/// `self.vec`. On [`Drop`] (and [`Self::finish`]), `self.vec.set_len(self.len)` commits the
/// initialized prefix.
pub struct ByteCursor<'a, A: Allocator> {
    vec: &'a mut Vec<u8, A>,
    base: *mut u8,
    len: usize,
    cap: usize,
}

impl<'a, A: Allocator> ByteCursor<'a, A> {
    /// Open a [`ByteCursor`] appending at the current end of `vec`.
    #[inline]
    pub fn new(vec: &'a mut Vec<u8, A>) -> Self {
        let base = vec.as_mut_ptr();
        let len = vec.len();
        let cap = vec.capacity();
        Self { vec, base, len, cap }
    }

    /// Number of initialized bytes currently in the underlying vector.
    #[inline(always)]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when zero bytes have been written.
    #[inline(always)]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total allocated capacity of the underlying vector in bytes.
    #[inline(always)]
    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }

    /// Allocated spare bytes remaining before a reallocation is required.
    #[inline(always)]
    #[must_use]
    pub const fn spare(&self) -> usize {
        self.cap - self.len
    }

    /// Borrow the initialized prefix `0..self.len`.
    #[inline(always)]
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `0..self.len` is initialized inside `self.base` (cursor invariant).
        unsafe { slice::from_raw_parts(self.base, self.len) }
    }

    /// Ensure at least `additional` spare bytes are available without zeroing them.
    #[inline(always)]
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        if additional <= self.cap - self.len {
            return Ok(());
        }
        self.reserve_cold(additional)
    }

    #[cold]
    #[inline(never)]
    fn reserve_cold(&mut self, additional: usize) -> Result<(), AllocFailure> {
        // SAFETY: `0..self.len` is initialized and `self.len <= self.vec.capacity()`.
        unsafe { self.vec.set_len(self.len) };
        self.vec.try_reserve(additional)?;
        self.base = self.vec.as_mut_ptr();
        self.cap = self.vec.capacity();
        Ok(())
    }

    /// Append a single byte if at least 1 spare byte is available. Returns `false` if out of room.
    #[inline(always)]
    pub fn push_within_cap(&mut self, byte: u8) -> bool {
        if self.len >= self.cap {
            return false;
        }
        // SAFETY: `self.len < self.cap`, so `self.base.add(self.len)` is valid spare capacity.
        unsafe { ptr::write(self.base.add(self.len), byte) };
        self.len += 1;
        true
    }

    /// Append a single byte, reserving capacity if needed.
    #[inline(always)]
    pub fn try_push(&mut self, byte: u8) -> Result<(), AllocFailure> {
        self.try_reserve(1)?;
        // SAFETY: `try_reserve(1)` guarantees `self.len < self.cap`.
        unsafe { ptr::write(self.base.add(self.len), byte) };
        self.len += 1;
        Ok(())
    }

    /// Append `bytes` if enough spare capacity is already reserved. Returns `false` otherwise.
    #[inline(always)]
    pub fn write_array_within_cap<const N: usize>(&mut self, bytes: [u8; N]) -> bool {
        if N > self.cap - self.len {
            return false;
        }
        // SAFETY: `N <= self.cap - self.len`, so `self.base.add(self.len)` has `N` writable bytes
        // non-overlapping with the stack array `bytes`.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(self.len), N) };
        self.len += N;
        true
    }

    /// Append all bytes of `src`, reserving capacity if needed.
    #[inline(always)]
    pub fn try_write_slice(&mut self, src: &[u8]) -> Result<(), AllocFailure> {
        let n = src.len();
        if n == 0 {
            return Ok(());
        }
        self.try_reserve(n)?;
        // SAFETY: `try_reserve(n)` guarantees `n <= self.cap - self.len`; `src` is an immutable
        // borrow distinct from `self.vec`'s exclusive buffer.
        unsafe { ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(self.len), n) };
        self.len += n;
        Ok(())
    }

    /// Append `count` copies of `byte` directly into spare capacity, reserving if needed.
    #[inline(always)]
    pub fn try_fill_bytes(&mut self, byte: u8, count: usize) -> Result<(), AllocFailure> {
        if count == 0 {
            return Ok(());
        }
        self.try_reserve(count)?;
        // SAFETY: `try_reserve(count)` guarantees `count <= self.cap - self.len`.
        unsafe { ptr::write_bytes(self.base.add(self.len), byte, count) };
        self.len += count;
        Ok(())
    }

    /// Copy `lit_len` bytes from `literals[lit_pos..]` into spare capacity using 16-byte / 32-byte
    /// wild copies when spare and source slack allow. Returns `false` if `literals` or spare
    /// capacity is insufficient.
    #[inline(always)]
    pub fn write_lits_wild(&mut self, literals: &[u8], lit_pos: usize, lit_len: usize) -> bool {
        if lit_len == 0 {
            return true;
        }
        let spare = self.cap - self.len;
        let Some(src_rem) = literals.len().checked_sub(lit_pos) else {
            return false;
        };
        if lit_len <= 16 && src_rem >= 16 && spare >= 16 {
            // SAFETY: `lit_pos + 16 <= literals.len()` and `self.len + 16 <= self.cap`; the 16
            // bytes written include `0..lit_len` (committed) and `lit_len..16` (harmless spare).
            unsafe {
                ptr::copy_nonoverlapping(literals.as_ptr().add(lit_pos), self.base.add(self.len), 16);
            }
            self.len += lit_len;
            return true;
        }
        if lit_len <= 32 && src_rem >= 32 && spare >= 32 {
            // SAFETY: `lit_pos + 32 <= literals.len()` and `self.len + 32 <= self.cap`.
            unsafe {
                ptr::copy_nonoverlapping(literals.as_ptr().add(lit_pos), self.base.add(self.len), 32);
            }
            self.len += lit_len;
            return true;
        }
        if lit_len > src_rem || lit_len > spare {
            return false;
        }
        // SAFETY: `lit_pos + lit_len <= literals.len()` and `self.len + lit_len <= self.cap`.
        unsafe {
            ptr::copy_nonoverlapping(literals.as_ptr().add(lit_pos), self.base.add(self.len), lit_len);
        }
        self.len += lit_len;
        true
    }

    /// Copy an LZ77 back-reference of `match_len` bytes from `self.len - offset` into spare
    /// capacity (`self.len..self.len + match_len`).
    ///
    /// Uses a single-branch 16-byte unconditional wildcopy loop when `offset >= 16` and at least
    /// `match_len + 16` spare bytes exist, NEON `memset` for `offset == 1`, and exponential
    /// doubling for overlapping short offsets. Returns `false` if `offset == 0`,
    /// `offset > self.len`, or `match_len > self.spare()`.
    #[inline(always)]
    pub fn copy_match_within_cap(&mut self, offset: usize, match_len: usize) -> bool {
        if offset == 0 || offset > self.len {
            return false;
        }
        let spare = self.cap - self.len;
        if match_len > spare {
            return false;
        }
        if match_len == 0 {
            return true;
        }

        // Fast path: `offset >= 16` with >= 16 bytes wildcopy slack -> single-branch 16-byte loop.
        if offset >= 16 && spare.saturating_sub(match_len) >= 16 {
            // SAFETY:
            // - `offset <= self.len` so `src` is inside initialized `0..self.len`.
            // - `dst - src == offset >= 16`, so every 16-byte chunk at `(src + k, dst + k)` is
            //   strictly non-overlapping and reads only initialized bytes (`src + k + 16 <= dst + k`).
            // - For any `match_len >= 1`, the final 16-byte write ends at `<= dst + match_len + 15
            //   < self.base + self.cap` because `spare >= match_len + 16`.
            unsafe {
                let mut src = self.base.add(self.len - offset);
                let mut dst = self.base.add(self.len);
                ptr::copy_nonoverlapping(src, dst, 16);
                if match_len > 16 {
                    let end = dst.add(match_len);
                    src = src.add(16);
                    dst = dst.add(16);
                    while dst < end {
                        ptr::copy_nonoverlapping(src, dst, 16);
                        src = src.add(16);
                        dst = dst.add(16);
                    }
                }
            }
            self.len += match_len;
            return true;
        }

        if offset >= 8 && match_len <= 16 && spare >= 16 {
            // SAFETY: `offset >= 8` makes the first 8-byte copy non-overlapping, and after it
            // `dst + 0..8` is initialized, making the second 8-byte copy (`src + 8..16 <= dst + 8`)
            // non-overlapping and reading only initialized bytes; `self.len + 16 <= self.cap`.
            unsafe {
                let src = self.base.add(self.len - offset);
                let dst = self.base.add(self.len);
                ptr::copy_nonoverlapping(src, dst, 8);
                if match_len > 8 {
                    ptr::copy_nonoverlapping(src.add(8), dst.add(8), 8);
                }
            }
            self.len += match_len;
            return true;
        }

        self.copy_match_cold(offset, match_len);
        true
    }

    #[cold]
    #[inline(never)]
    fn copy_match_cold(&mut self, offset: usize, match_len: usize) {
        // SAFETY: Caller verified `1 <= offset <= self.len` and `match_len <= self.cap - self.len`.
        unsafe {
            let src = self.base.add(self.len - offset);
            let dst = self.base.add(self.len);
            if offset == 1 {
                let byte = *src;
                ptr::write_bytes(dst, byte, match_len);
            } else if offset >= match_len {
                ptr::copy_nonoverlapping(src, dst, match_len);
            } else {
                ptr::copy_nonoverlapping(src, dst, offset);
                let mut copied = offset;
                while copied < match_len {
                    let chunk = copied.min(match_len - copied);
                    ptr::copy_nonoverlapping(dst, dst.add(copied), chunk);
                    copied += chunk;
                }
            }
        }
        self.len += match_len;
    }

    /// Write an 8-byte little-endian word into spare capacity and advance `self.len` by `adv`
    /// (`adv <= 8`). Enables branchless single-store bitstream flushes (`adv = bits >> 3`).
    /// Returns `false` if `adv > 8` or fewer than 8 spare bytes remain.
    #[inline(always)]
    pub fn write_u64_le_adv(&mut self, word: u64, adv: usize) -> bool {
        if adv > 8 || self.cap - self.len < 8 {
            return false;
        }
        // SAFETY: `self.len + 8 <= self.cap`, so `self.base.add(self.len)` has 8 writable spare
        // bytes; advancing by `adv <= 8` commits the `adv` filled bytes into `0..self.len`.
        unsafe {
            ptr::write_unaligned(self.base.add(self.len).cast::<u64>(), word.to_le());
        }
        self.len += adv;
        true
    }

    /// Fused Zstandard sequence execution (`literals` + `match`) in a single bounds check:
    /// uses 128-bit NEON wildcopy for both literals (`<= 16` single copy or `> 16` 16B loop) and
    /// matches (`offset >= 16` single copy or `> 16` 16B loop) without bouncing through separate
    /// bounds checks.
    ///
    /// Returns `true` on success, `false` if bounds/capacity are violated.
    #[inline(always)]
    pub fn exec_seq_within_cap(
        &mut self,
        literals: &[u8],
        lit_pos: usize,
        lit_len: usize,
        offset: usize,
        match_len: usize,
    ) -> bool {
        let Some(src_rem) = literals.len().checked_sub(lit_pos) else {
            return false;
        };
        if lit_len > src_rem {
            return false;
        }
        let Some(seq_out) = lit_len.checked_add(match_len) else {
            return false;
        };
        let spare = self.cap - self.len;
        if seq_out > spare {
            return false;
        }
        let after_lit = self.len + lit_len;
        if offset == 0 || offset > after_lit {
            return false;
        }

        if lit_len <= 16 && match_len <= 16 && offset >= 16 && spare >= 48 && src_rem >= 16 {
            // SAFETY:
            // - `lit_pos + 16 <= literals.len()` (`src_rem >= 16`) and `self.len + 16 <= self.cap`.
            // - `offset >= 16` and `offset <= after_lit`, so `after_lit - offset` has 16 readable
            //   initialized bytes strictly non-overlapping with `after_lit`.
            // - `after_lit + 16 <= self.len + 32 < self.cap` (`spare >= 48`).
            unsafe {
                ptr::copy_nonoverlapping(literals.as_ptr().add(lit_pos), self.base.add(self.len), 16);
                ptr::copy_nonoverlapping(
                    self.base.add(after_lit - offset),
                    self.base.add(after_lit),
                    16,
                );
            }
            self.len = after_lit + match_len;
            return true;
        }

        // Pass fields by value so `&mut self` never escapes into the cold helper (preserves SROA).
        self.len = exec_seq_general_raw(
            self.base,
            self.len,
            self.cap,
            literals,
            lit_pos,
            lit_len,
            offset,
            match_len,
            src_rem,
            spare,
        );
        true
    }

    /// Call-free inline fast path for executing one decoded LZ77 sequence inside a zero-call
    /// fused decoder loop (`decode_exec_leaf`).
    ///
    /// Executes inline with 16B/32B NEON wildcopies (plus 8B/16B inline copies for `8 <= offset
    /// < 16` and an inline 16B match wildcopy loop for `match_len > 32`) whenever `lit_len <= 32`,
    /// `lit_pos <= literals.len().saturating_sub(32)`, `after_lit + match_len <=
    /// self.cap.saturating_sub(48)`, and `offset <= after_lit`.
    /// Returns `false` without mutating `self.len` on rare cases (`lit_len > 32`, `offset < 8`,
    /// `offset < 16 && match_len > 16`, or final 32B literal slack) so the caller can dispatch
    /// that single sequence out-of-line without placing any `bl` call inside the hot leaf loop.
    #[inline(always)]
    pub fn try_exec_seq_leaf(
        &mut self,
        literals: &[u8],
        lit_pos: usize,
        lit_len: usize,
        offset: usize,
        match_len: usize,
    ) -> bool {
        let lit_safe_end = literals.len().saturating_sub(32);
        let out_safe_end = self.cap.saturating_sub(48);
        let len = self.len;
        let after_lit = len + lit_len;
        let next_out = after_lit + match_len;
        if lit_len > 32
            || lit_pos > lit_safe_end
            || next_out > out_safe_end
            || offset == 0
            || offset > after_lit
        {
            return false;
        }
        if offset < 16 {
            if offset < 8 || match_len > 16 {
                return false;
            }
            // SAFETY:
            // - `lit_pos + 32 <= literals.len()` and `len + 32 < self.cap`.
            // - `8 <= offset <= after_lit`, `match_len <= 16`, and `after_lit + 16 < self.cap`,
            //   so each 8B copy is non-overlapping and reads initialized history.
            unsafe {
                let ls = literals.as_ptr().add(lit_pos);
                let ld = self.base.add(len);
                ptr::copy_nonoverlapping(ls, ld, 16);
                if lit_len > 16 {
                    ptr::copy_nonoverlapping(ls.add(16), ld.add(16), 16);
                }
                let ms = self.base.add(after_lit - offset);
                let md = self.base.add(after_lit);
                ptr::copy_nonoverlapping(ms, md, 8);
                if match_len > 8 {
                    ptr::copy_nonoverlapping(ms.add(8), md.add(8), 8);
                }
            }
            self.len = next_out;
            return true;
        }
        // SAFETY:
        // - `lit_pos + 32 <= literals.len()` and `len + 32 < self.cap`, so 1x16B and 2x16B
        //   wildcopies of literals stay strictly in bounds and non-overlapping.
        // - `16 <= offset <= after_lit` and `next_out + 48 <= self.cap`, so every 16B match
        //   wildcopy chunk reads initialized history non-overlapping with `dst` and writes
        //   strictly inside `self.base..self.base + self.cap`.
        unsafe {
            let ls = literals.as_ptr().add(lit_pos);
            let ld = self.base.add(len);
            ptr::copy_nonoverlapping(ls, ld, 16);
            if lit_len > 16 {
                ptr::copy_nonoverlapping(ls.add(16), ld.add(16), 16);
            }

            let ms = self.base.add(after_lit - offset);
            let md = self.base.add(after_lit);
            ptr::copy_nonoverlapping(ms, md, 16);
            if match_len > 16 {
                ptr::copy_nonoverlapping(ms.add(16), md.add(16), 16);
                if match_len > 32 {
                    let mut sp = ms.add(32);
                    let mut dp = md.add(32);
                    let end = md.add(match_len);
                    while dp < end {
                        ptr::copy_nonoverlapping(sp, dp, 16);
                        sp = sp.add(16);
                        dp = dp.add(16);
                    }
                }
            }
        }
        self.len = next_out;
        true
    }

    /// Execute a batch of decoded LZ77 sequences (`&[SeqOp]`) back-to-back in a tight
    /// register-resident 128-bit NEON wildcopy loop.
    ///
    /// Pre-verifies the entire batch's literal and output slack once up front so the 99% hot
    /// loop runs with zero per-sequence bounds checks, inline 16B/32B NEON literal & match
    /// wildcopies (never calling `_memcpy`), and a single unsigned offset check.
    ///
    /// Returns `false` if any sequence violates `literals`, `self.spare()`, or `offset` bounds.
    #[inline(never)]
    pub fn exec_seq_batch(
        &mut self,
        literals: &[u8],
        lit_pos: &mut usize,
        seqs: &[SeqOp],
    ) -> bool {
        let base = self.base;
        let cap = self.cap;
        let mut len = self.len;
        let lit_base = literals.as_ptr();
        let lit_total = literals.len();
        let mut lp = *lit_pos;
        if lp > lit_total {
            return false;
        }

        // Verify once whether the entire batch has >= 32B literal slack and >= 48B output slack.
        // For batches `<= 64`, `sum_lits` and `sum_out` in `u64` cannot overflow (`<= 128 * u32::MAX`).
        let mut sum_lits_u64: u64 = 0;
        let mut sum_out_u64: u64 = 0;
        let fits_fast = seqs.len() <= 64;
        if fits_fast {
            for seq in seqs {
                let ll = u64::from(seq.lit_len);
                let ml = u64::from(seq.match_len);
                sum_lits_u64 += ll;
                sum_out_u64 += ll + ml;
            }
        }
        let (Ok(sum_lits), Ok(sum_out)) =
            (usize::try_from(sum_lits_u64), usize::try_from(sum_out_u64))
        else {
            return self.exec_seq_batch_tail(literals, lit_pos, seqs);
        };

        let lit_slack_ok = fits_fast && lit_total - lp >= sum_lits.saturating_add(32);
        let out_slack_ok = fits_fast && cap - len >= sum_out.saturating_add(48);
        if lit_slack_ok && out_slack_ok {
            for seq in seqs {
                let Ok(lit_len) = usize::try_from(seq.lit_len) else {
                    self.len = len;
                    *lit_pos = lp;
                    return false;
                };
                let Ok(match_len) = usize::try_from(seq.match_len) else {
                    self.len = len;
                    *lit_pos = lp;
                    return false;
                };
                let Ok(offset) = usize::try_from(seq.offset) else {
                    self.len = len;
                    *lit_pos = lp;
                    return false;
                };

                // 1. Copy literals: 1x16B for <= 16, 2x16B for 17..=32, cold helper for > 32.
                // SAFETY: Batch pre-check guarantees `lp + lit_len + 32 <= lit_total` and
                // `len + lit_len + 48 <= cap`.
                unsafe {
                    let s = lit_base.add(lp);
                    let d = base.add(len);
                    ptr::copy_nonoverlapping(s, d, 16);
                    if lit_len > 16 {
                        ptr::copy_nonoverlapping(s.add(16), d.add(16), 16);
                        if lit_len > 32 {
                            copy_lit_tail_raw(s.add(32), d.add(32), lit_len - 32);
                        }
                    }
                }
                lp += lit_len;
                let after_lit = len + lit_len;

                // Single unsigned compare rejects both `offset == 0` and `offset > after_lit`.
                if offset.wrapping_sub(1) >= after_lit {
                    self.len = after_lit;
                    *lit_pos = lp;
                    return false;
                }

                // 2. Copy match: 1x16B for <= 16, 2x16B for 17..=32, wildcopy loop for > 32.
                if offset >= 16 {
                    // SAFETY: `16 <= offset <= after_lit` and `after_lit + match_len + 48 <= cap`.
                    unsafe {
                        let s = base.add(after_lit - offset);
                        let d = base.add(after_lit);
                        ptr::copy_nonoverlapping(s, d, 16);
                        if match_len > 16 {
                            ptr::copy_nonoverlapping(s.add(16), d.add(16), 16);
                            if match_len > 32 {
                                let mut sp = s.add(32);
                                let mut dp = d.add(32);
                                let end = d.add(match_len);
                                while dp < end {
                                    ptr::copy_nonoverlapping(sp, dp, 16);
                                    sp = sp.add(16);
                                    dp = dp.add(16);
                                }
                            }
                        }
                    }
                } else if offset >= 8 && match_len <= 16 {
                    // SAFETY: `8 <= offset <= after_lit` and `after_lit + 16 <= cap`.
                    unsafe {
                        let s = base.add(after_lit - offset);
                        let d = base.add(after_lit);
                        ptr::copy_nonoverlapping(s, d, 8);
                        if match_len > 8 {
                            ptr::copy_nonoverlapping(s.add(8), d.add(8), 8);
                        }
                    }
                } else if match_len > 0 {
                    copy_match_overlap_raw(base, after_lit, offset, match_len);
                }
                len = after_lit + match_len;
            }
            self.len = len;
            *lit_pos = lp;
            return true;
        }

        self.exec_seq_batch_tail(literals, lit_pos, seqs)
    }

    #[cold]
    #[inline(never)]
    fn exec_seq_batch_tail(
        &mut self,
        literals: &[u8],
        lit_pos: &mut usize,
        seqs: &[SeqOp],
    ) -> bool {
        let base = self.base;
        let cap = self.cap;
        let mut len = self.len;
        let lit_base = literals.as_ptr();
        let lit_total = literals.len();
        let mut lp = *lit_pos;
        if lp > lit_total {
            return false;
        }
        // Tail fallback for the final batch of a block where < 32B literal slack remains.
        for seq in seqs {
            let Ok(lit_len) = usize::try_from(seq.lit_len) else {
                self.len = len;
                *lit_pos = lp;
                return false;
            };
            let Ok(match_len) = usize::try_from(seq.match_len) else {
                self.len = len;
                *lit_pos = lp;
                return false;
            };
            let Ok(offset) = usize::try_from(seq.offset) else {
                self.len = len;
                *lit_pos = lp;
                return false;
            };

            let src_rem = lit_total - lp;
            let spare = cap - len;
            let Some(seq_out) = lit_len.checked_add(match_len) else {
                self.len = len;
                *lit_pos = lp;
                return false;
            };
            if lit_len > src_rem || seq_out > spare {
                self.len = len;
                *lit_pos = lp;
                return false;
            }
            let after_lit = len + lit_len;
            if offset.wrapping_sub(1) >= after_lit {
                self.len = len;
                *lit_pos = lp;
                return false;
            }

            if lit_len <= 16 && src_rem >= 16 && spare >= 16 {
                // SAFETY: `lp + 16 <= lit_total` and `len + 16 <= cap`.
                unsafe {
                    ptr::copy_nonoverlapping(lit_base.add(lp), base.add(len), 16);
                }
            } else if lit_len > 0 {
                // SAFETY: `lp + lit_len <= lit_total` and `len + lit_len <= cap`.
                unsafe {
                    copy_lit_tail_raw(lit_base.add(lp), base.add(len), lit_len);
                }
            }
            lp += lit_len;

            let spare_m = cap - after_lit;
            if offset >= 16 && spare_m - match_len >= 16 {
                // SAFETY: `16 <= offset <= after_lit` and `spare_m >= match_len + 16`.
                unsafe {
                    let mut s = base.add(after_lit - offset);
                    let mut d = base.add(after_lit);
                    ptr::copy_nonoverlapping(s, d, 16);
                    if match_len > 16 {
                        let end = d.add(match_len);
                        s = s.add(16);
                        d = d.add(16);
                        while d < end {
                            ptr::copy_nonoverlapping(s, d, 16);
                            s = s.add(16);
                            d = d.add(16);
                        }
                    }
                }
            } else if offset >= 8 && match_len <= 16 && spare_m >= 16 {
                // SAFETY: `8 <= offset <= after_lit` and `after_lit + 16 <= cap`.
                unsafe {
                    let s = base.add(after_lit - offset);
                    let d = base.add(after_lit);
                    ptr::copy_nonoverlapping(s, d, 8);
                    if match_len > 8 {
                        ptr::copy_nonoverlapping(s.add(8), d.add(8), 8);
                    }
                }
            } else if match_len > 0 {
                copy_match_overlap_raw(base, after_lit, offset, match_len);
            }
            len = after_lit + match_len;
        }

        self.len = len;
        *lit_pos = lp;
        true
    }

    /// Commit the written length back into the underlying [`Vec<u8, A>`].
    #[inline(always)]
    pub fn finish(self) {
        drop(self);
    }
}

#[cold]
#[inline(never)]
unsafe fn copy_lit_tail_raw(src: *const u8, dst: *mut u8, len: usize) {
    // SAFETY: Caller verified `src..src + len` and `dst..dst + len` are valid and non-overlapping.
    unsafe {
        ptr::copy_nonoverlapping(src, dst, len);
    }
}

#[cold]
#[inline(never)]
fn copy_match_overlap_raw(base: *mut u8, after_lit: usize, offset: usize, match_len: usize) {
    // SAFETY: Caller verified `1 <= offset <= after_lit` and `after_lit + match_len <= cap`.
    unsafe {
        let src = base.add(after_lit - offset);
        let dst = base.add(after_lit);
        if offset == 1 {
            let byte = *src;
            ptr::write_bytes(dst, byte, match_len);
        } else if offset >= match_len {
            ptr::copy_nonoverlapping(src, dst, match_len);
        } else {
            ptr::copy_nonoverlapping(src, dst, offset);
            let mut copied = offset;
            while copied < match_len {
                let chunk = copied.min(match_len - copied);
                ptr::copy_nonoverlapping(dst, dst.add(copied), chunk);
                copied += chunk;
            }
        }
    }
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn exec_seq_general_raw(
    base: *mut u8,
    len: usize,
    cap: usize,
    literals: &[u8],
    lit_pos: usize,
    lit_len: usize,
    offset: usize,
    match_len: usize,
    src_rem: usize,
    spare: usize,
) -> usize {
    let after_lit = len + lit_len;
    // 1. Copy literals into `len..after_lit` (16B NEON wildcopy when slack allows).
    if lit_len <= 16 && src_rem >= 16 && spare >= 16 {
        // SAFETY: `lit_pos + 16 <= literals.len()` and `len + 16 <= cap`.
        unsafe {
            ptr::copy_nonoverlapping(literals.as_ptr().add(lit_pos), base.add(len), 16);
        }
    } else if lit_len > 0 {
        if src_rem.saturating_sub(lit_len) >= 16 && spare.saturating_sub(lit_len) >= 16 {
            // SAFETY: Both source and destination have at least `lit_len + 16` accessible
            // bytes, so 16-byte wildcopy chunks stay strictly in bounds and non-overlapping.
            unsafe {
                let mut src = literals.as_ptr().add(lit_pos);
                let mut dst = base.add(len);
                let end = dst.add(lit_len);
                while dst < end {
                    ptr::copy_nonoverlapping(src, dst, 16);
                    src = src.add(16);
                    dst = dst.add(16);
                }
            }
        } else {
            // SAFETY: `lit_pos + lit_len <= literals.len()` and `len + lit_len <= cap`.
            unsafe {
                ptr::copy_nonoverlapping(literals.as_ptr().add(lit_pos), base.add(len), lit_len);
            }
        }
    }

    // 2. Copy match into `after_lit..after_lit + match_len`.
    let spare_match = cap - after_lit;
    if offset >= 16 && spare_match.saturating_sub(match_len) >= 16 {
        // SAFETY:
        // - `offset <= after_lit` and `offset >= 16`, so `src = after_lit - offset` is
        //   strictly non-overlapping with `dst = after_lit` for every 16-byte chunk.
        // - `spare_match >= match_len + 16`, so the final 16-byte wild write stays `< cap`.
        unsafe {
            let mut src = base.add(after_lit - offset);
            let mut dst = base.add(after_lit);
            ptr::copy_nonoverlapping(src, dst, 16);
            if match_len > 16 {
                let end = dst.add(match_len);
                src = src.add(16);
                dst = dst.add(16);
                while dst < end {
                    ptr::copy_nonoverlapping(src, dst, 16);
                    src = src.add(16);
                    dst = dst.add(16);
                }
            }
        }
        return after_lit + match_len;
    }
    if offset >= 8 && match_len <= 16 && spare_match >= 16 {
        // SAFETY: `offset >= 8` makes each 8-byte copy non-overlapping and reading only
        // initialized bytes; `after_lit + 16 <= cap`.
        unsafe {
            let src = base.add(after_lit - offset);
            let dst = base.add(after_lit);
            ptr::copy_nonoverlapping(src, dst, 8);
            if match_len > 8 {
                ptr::copy_nonoverlapping(src.add(8), dst.add(8), 8);
            }
        }
        return after_lit + match_len;
    }
    if match_len > 0 {
        // SAFETY: Caller verified `1 <= offset <= after_lit` and `match_len <= cap - after_lit`.
        unsafe {
            let src = base.add(after_lit - offset);
            let dst = base.add(after_lit);
            if offset == 1 {
                let byte = *src;
                ptr::write_bytes(dst, byte, match_len);
            } else if offset >= match_len {
                ptr::copy_nonoverlapping(src, dst, match_len);
            } else {
                ptr::copy_nonoverlapping(src, dst, offset);
                let mut copied = offset;
                while copied < match_len {
                    let chunk = copied.min(match_len - copied);
                    ptr::copy_nonoverlapping(dst, dst.add(copied), chunk);
                    copied += chunk;
                }
            }
        }
    }
    after_lit + match_len
}

impl<A: Allocator> Drop for ByteCursor<'_, A> {
    #[inline(always)]
    fn drop(&mut self) {
        // SAFETY: `0..self.len` is initialized inside `self.vec`'s buffer (cursor invariant).
        unsafe { self.vec.set_len(self.len) };
    }
}

/// Safe pointer-based reverse bitstream reader for RFC 8878 Zstandard (Huffman & FSE).
///
/// Maintains `self.start <= self.ptr`, and whenever `self.ptr > self.start` the original borrowed
/// slice has length `>= 8` with `self.ptr..self.ptr + 8` strictly within that slice.
#[derive(Clone, Copy)]
pub struct RevBitCursor<'a> {
    start: *const u8,
    ptr: *const u8,
    container: u64,
    bits_consumed: u32,
    _marker: PhantomData<&'a [u8]>,
}

impl<'a> RevBitCursor<'a> {
    /// Open a reverse bitstream, stripping the high padding zeros and sentinel `1` bit in the
    /// final byte. Returns `None` if `bytes` is empty or its final byte is `0`.
    #[inline(always)]
    pub fn new(bytes: &'a [u8]) -> Option<Self> {
        let &last = bytes.last()?;
        if last == 0 {
            return None;
        }
        let leading = last.leading_zeros();
        let n = bytes.len();
        let start = bytes.as_ptr();
        if n >= 8 {
            // SAFETY: `n >= 8`, so `start.add(n - 8)` points to 8 readable bytes in `bytes`.
            let ptr = unsafe { start.add(n - 8) };
            // SAFETY: `ptr..ptr + 8` is inside `bytes`.
            let container = u64::from_le(unsafe { ptr::read_unaligned(ptr.cast::<u64>()) });
            Some(Self {
                start,
                ptr,
                container,
                bits_consumed: leading + 1,
                _marker: PhantomData,
            })
        } else {
            let mut buf = [0u8; 8];
            if let Some(dst) = buf.get_mut(..n) {
                dst.copy_from_slice(bytes);
            }
            let unused_high = u32::try_from(8 - n).ok()?;
            Some(Self {
                start,
                ptr: start,
                container: u64::from_le_bytes(buf),
                bits_consumed: unused_high * 8 + leading + 1,
                _marker: PhantomData,
            })
        }
    }

    /// Unread bytes strictly below `self.ptr` (`0` once the tail window is reached).
    #[inline(always)]
    #[must_use]
    pub fn avail_bytes(&self) -> usize {
        // SAFETY: `self.ptr >= self.start` within the same slice allocation (struct invariant).
        unsafe { self.ptr.offset_from_unsigned(self.start) }
    }

    /// Number of top bits already consumed from `container` (`0..=64`).
    #[inline(always)]
    #[must_use]
    pub const fn bits_consumed(&self) -> u32 {
        self.bits_consumed
    }

    /// Bulk-refill `container` in one unaligned 64-bit load.
    ///
    /// Returns `true` on the fast path (`self.bits_consumed <= 7`, at least 57 bits live, and
    /// `self.ptr > self.start` still holds). Returns `false` when the stream reaches the final
    /// `<= 8` bytes (tail window); `container` is still updated to the exact tail state.
    #[inline(always)]
    pub fn reload(&mut self) -> bool {
        // SAFETY: `self.ptr >= self.start` (struct invariant).
        let avail = unsafe { self.ptr.offset_from_unsigned(self.start) };
        let [lo, ..] = self.bits_consumed.to_le_bytes();
        let adv = usize::from(lo >> 3);
        if avail > adv {
            // SAFETY: `avail > adv >= 0` guarantees `avail >= 1` (hence original `len >= 8` and
            // `ptr` only moves down from `end - 8`) and `self.ptr.sub(adv) > self.start`, so
            // `new_ptr..new_ptr + 8` is strictly within the borrowed slice.
            unsafe {
                self.ptr = self.ptr.sub(adv);
                self.container = u64::from_le(ptr::read_unaligned(self.ptr.cast::<u64>()));
            }
            self.bits_consumed &= 7;
            true
        } else {
            if avail > 0 {
                // SAFETY: `avail > 0` implies original `len >= 8`, so `self.start..self.start + 8`
                // is valid and readable; passing fields by value keeps `RevBitCursor` in registers
                // (preserves LLVM SROA).
                let (container, bits) = reload_tail_val(self.start, avail, self.bits_consumed);
                self.ptr = self.start;
                self.container = container;
                self.bits_consumed = bits;
            }
            false
        }
    }

    /// Call-free, branch-minimal reload for loops that guard `self.avail_bytes() >= 8` and
    /// `self.bits_consumed() < 64`. Because `bits_consumed >> 3 <= 7 < 8 <= avail`, this compiles
    /// to a single 4-instruction unaligned load with no clamping or tail call.
    /// Returns `false` if `< 8` unread bytes remain or `bits_consumed >= 64`.
    #[inline(always)]
    pub fn reload_if_slack8(&mut self) -> bool {
        let avail = self.avail_bytes();
        if avail < 8 || self.bits_consumed >= 64 {
            return false;
        }
        let [lo, ..] = self.bits_consumed.to_le_bytes();
        let adv = usize::from(lo >> 3);
        // SAFETY: `adv <= 7 < 8 <= avail = ptr - start`, so `ptr.sub(adv) > start` and
        // `ptr.sub(adv)..ptr.sub(adv) + 8` lies strictly within the borrowed slice.
        unsafe {
            self.ptr = self.ptr.sub(adv);
            self.container = u64::from_le(ptr::read_unaligned(self.ptr.cast::<u64>()));
        }
        self.bits_consumed &= 7;
        true
    }

    /// Call-free, branch-minimal reload for loops that already guard `self.avail_bytes() >= 16`.
    /// Clamps safely to `self.start` if called with less slack, with zero out-of-line calls so
    /// LLVM keeps every hot cursor register live with zero spills.
    #[inline(always)]
    pub fn reload_fast(&mut self) {
        let avail = self.avail_bytes();
        if avail > 0 {
            let [lo, ..] = self.bits_consumed.to_le_bytes();
            let adv = usize::from(lo >> 3);
            let step = adv.min(avail);
            // SAFETY: `avail > 0` implies original `len >= 8`, and `step <= avail = ptr - start`,
            // so `self.ptr.sub(step)..self.ptr.sub(step) + 8` lies strictly inside the slice.
            unsafe {
                self.ptr = self.ptr.sub(step);
                self.container = u64::from_le(ptr::read_unaligned(self.ptr.cast::<u64>()));
            }
            let step_bits = u32::try_from(step).unwrap_or(0) << 3;
            self.bits_consumed = self.bits_consumed.saturating_sub(step_bits);
        }
    }

    /// Return the current bit container shifted left by `bits_consumed & 63` so the next unread
    /// bits sit at the MSB (`63` down). Enables parallel multi-field bit extraction after a
    /// verified reload (`bits_consumed <= 7`, `>= 57` live bits).
    #[inline(always)]
    #[must_use]
    pub const fn bits_window(&self) -> u64 {
        self.container.wrapping_shl(self.bits_consumed)
    }

    /// Branchless peek of `64 - shift_r` bits (`1..=16`). Caller must ensure `bits_consumed < 64`.
    #[inline(always)]
    #[must_use]
    pub fn peek_fast(&self, shift_r: u32) -> usize {
        let shifted = (self.container << self.bits_consumed) >> shift_r;
        let [b0, b1, ..] = shifted.to_le_bytes();
        usize::from(u16::from_le_bytes([b0, b1]))
    }

    /// Branchless advance of `n` bits after a verified fast peek.
    #[inline(always)]
    pub const fn advance_fast(&mut self, n: u32) {
        self.bits_consumed += n;
    }

    /// Branchless read of `n` bits (`0..=32`). Caller must ensure `bits_consumed < 64` and
    /// `bits_consumed + n <= 64`.
    #[inline(always)]
    pub fn read_bits_branchless(&mut self, n: u32) -> u32 {
        let shifted = ((self.container << self.bits_consumed) >> 1) >> (63 - n);
        self.bits_consumed += n;
        let [b0, b1, b2, b3, ..] = shifted.to_le_bytes();
        u32::from_le_bytes([b0, b1, b2, b3])
    }

    /// Read exactly `n` bits (`0..=32`), reloading if needed.
    #[inline(always)]
    pub fn read_bits(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if n > 32 {
            return None;
        }
        if self.bits_consumed + n > 64 {
            let _ = self.reload();
        }
        let next = self.bits_consumed + n;
        if next > 64 {
            return None;
        }
        let shifted = (self.container << self.bits_consumed) >> (64 - n);
        self.bits_consumed = next;
        let [b0, b1, b2, b3, ..] = shifted.to_le_bytes();
        Some(u32::from_le_bytes([b0, b1, b2, b3]))
    }

    /// Peek `n` bits (`1..=11`) for Huffman tail lookup, right-padding with zeros near stream end.
    #[inline(always)]
    pub fn peek_bits_padded(&mut self, n: u32) -> u32 {
        if self.bits_consumed + n > 64 && self.avail_bytes() > 0 {
            let _ = self.reload();
        }
        if self.bits_consumed >= 64 {
            return 0;
        }
        let shifted = (self.container << self.bits_consumed) >> (64 - n);
        let [b0, b1, ..] = shifted.to_le_bytes();
        u32::from(u16::from_le_bytes([b0, b1]))
    }

    /// Consume `n` bits (`0..=32`), returning `false` if fewer than `n` bits remain.
    #[inline(always)]
    pub fn consume_bits(&mut self, n: u32) -> bool {
        if n == 0 {
            return true;
        }
        if self.bits_consumed + n > 64 {
            let _ = self.reload();
        }
        let next = self.bits_consumed + n;
        if next > 64 {
            return false;
        }
        self.bits_consumed = next;
        true
    }

    /// Returns `true` when every bit in the reverse bitstream has been consumed.
    #[inline(always)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ptr == self.start && self.bits_consumed == 64
    }
}

#[cold]
#[inline(never)]
fn reload_tail_val(start: *const u8, avail: usize, bits_consumed: u32) -> (u64, u32) {
    let step = u32::try_from(avail).unwrap_or(0);
    let bits = bits_consumed.saturating_sub(step * 8);
    // SAFETY: Caller only invokes this when `avail > 0`, which guarantees `len >= 8`.
    let container = u64::from_le(unsafe { ptr::read_unaligned(start.cast::<u64>()) });
    (container, bits)
}

/// Safe pointer-based writer over a fixed `&'a mut [u8]` slice for batched 64-bit stores
/// (e.g., 4-stream `HUF_4X2` Huffman decoding).
pub struct SliceWriter<'a> {
    ptr: *mut u8,
    end: *mut u8,
    _marker: PhantomData<&'a mut [u8]>,
}

impl<'a> SliceWriter<'a> {
    /// Wrap a mutable byte slice for pointer-resident writes.
    #[inline(always)]
    pub fn new(slice: &'a mut [u8]) -> Self {
        let len = slice.len();
        let ptr = slice.as_mut_ptr();
        // SAFETY: `ptr.add(len)` is the one-past-end pointer of `slice`.
        let end = unsafe { ptr.add(len) };
        Self { ptr, end, _marker: PhantomData }
    }

    /// Remaining unwritten bytes in the slice.
    #[inline(always)]
    #[must_use]
    pub fn rem(&self) -> usize {
        // SAFETY: `self.ptr <= self.end` within the same slice (struct invariant).
        unsafe { self.end.offset_from_unsigned(self.ptr) }
    }

    /// Returns `true` when at least 8 bytes remain for a batched `u64` store.
    #[inline(always)]
    #[must_use]
    pub fn has_8(&self) -> bool {
        self.rem() >= 8
    }

    /// Store `word` in little-endian order and advance the cursor by `adv` (`adv <= 8`).
    /// Returns `false` if `adv > 8` or fewer than 8 bytes remain.
    #[inline(always)]
    pub fn write_u64_le_adv(&mut self, word: u64, adv: usize) -> bool {
        if adv > 8 || self.rem() < 8 {
            return false;
        }
        // SAFETY: `self.ptr..self.ptr + 8 <= self.end` and `adv <= 8`.
        unsafe {
            ptr::write_unaligned(self.ptr.cast::<u64>(), word.to_le());
            self.ptr = self.ptr.add(adv);
        }
        true
    }

    /// Consume the writer and return the remaining unwritten tail slice.
    #[inline(always)]
    #[must_use]
    pub fn into_rest(self) -> &'a mut [u8] {
        let len = self.rem();
        // SAFETY: `self.ptr..self.end` is the remaining exclusive sub-slice of the original `&mut [u8]`.
        unsafe { slice::from_raw_parts_mut(self.ptr, len) }
    }
}

/// Fast 8-byte SWAR common-prefix length between `input[a..]` and `input[b..limit_end]`.
///
/// Used by LZ77 compressors (`bstd::zstd`, `bstd::lz4`, `bstd::snappy`) to measure match length
/// in 64-bit chunks without slice-iterator bounds checks.
#[inline(always)]
#[must_use]
pub fn common_prefix_len(input: &[u8], a: usize, b: usize, limit_end: usize) -> usize {
    let end = limit_end.min(input.len());
    if a >= input.len() || b >= end {
        return 0;
    }
    let max_len = (input.len() - a).min(end - b);
    let base = input.as_ptr();
    // SAFETY: `a + max_len <= input.len()` and `b + max_len <= end <= input.len()`.
    unsafe {
        let mut pa = base.add(a);
        let mut pb = base.add(b);
        let mut rem = max_len;
        let mut len: usize = 0;
        while rem >= 8 {
            let va = u64::from_le(ptr::read_unaligned(pa.cast::<u64>()));
            let vb = u64::from_le(ptr::read_unaligned(pb.cast::<u64>()));
            let diff = va ^ vb;
            if diff != 0 {
                return len + usize::try_from(diff.trailing_zeros() >> 3).unwrap_or(0);
            }
            pa = pa.add(8);
            pb = pb.add(8);
            len += 8;
            rem -= 8;
        }
        while rem > 0 && *pa == *pb {
            pa = pa.add(1);
            pb = pb.add(1);
            len += 1;
            rem -= 1;
        }
        len
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::heap;
    use crate::Vec;

    #[test]
    fn byte_cursor_wild_lits_and_matches_all_offsets() {
        let h = heap();
        let alloc = h.alloc();
        let mut out = Vec::try_with_capacity_in(1024, alloc).expect("cap");
        let lits = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

        {
            let mut cur = out.byte_cursor();
            assert!(cur.write_lits_wild(lits, 0, 20)); // >16, <=32 wild
            assert!(cur.write_lits_wild(lits, 20, 5)); // <=16 wild
            // Non-overlapping >=16 wild match (len <= 16 and len > 16)
            assert!(cur.copy_match_within_cap(20, 10));
            assert!(cur.copy_match_within_cap(20, 35));
            // Offset 8..15 inline path
            assert!(cur.copy_match_within_cap(8, 13));
            // RLE offset == 1
            assert!(cur.try_push(b'!').is_ok());
            assert!(cur.copy_match_within_cap(1, 24));
            // Overlapping repeat offset == 3
            assert!(cur.try_write_slice(b"abc").is_ok());
            assert!(cur.copy_match_within_cap(3, 19));
            cur.finish();
        }

        // Verify against reference scalar construction.
        let mut expected = Vec::new_in(alloc);
        expected.extend_from_slice(&lits[..25]);
        for &(off, len) in &[(20usize, 10usize), (20, 35), (8, 13)] {
            for _ in 0..len {
                let b = expected[expected.len() - off];
                expected.push(b);
            }
        }
        expected.push(b'!');
        for _ in 0..24 {
            expected.push(b'!');
        }
        expected.extend_from_slice(b"abc");
        for _ in 0..19 {
            let b = expected[expected.len() - 3];
            expected.push(b);
        }
        assert_eq!(out.as_slice(), expected.as_slice());
    }

    #[test]
    fn rev_bit_cursor_slice_writer_and_common_prefix_len() {
        use super::{RevBitCursor, SliceWriter, common_prefix_len};

        // 1. RevBitCursor across > 8 bytes + tail reload.
        // Byte stream: 12 bytes with sentinel bit at MSB of last byte (0x80).
        let stream: [u8; 12] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0x80];
        let mut rbc = RevBitCursor::new(&stream).expect("valid stream");
        assert!(rbc.avail_bytes() == 4);
        // Top 7 bits of 0x80 are 0, then 0xBB, 0xAA, 0x99...
        assert_eq!(rbc.read_bits(7), Some(0));
        assert!(rbc.reload());
        assert_eq!(rbc.read_bits_branchless(8), 0xBB);
        assert_eq!(rbc.read_bits_branchless(8), 0xAA);
        assert_eq!(rbc.read_bits_branchless(8), 0x99);
        assert_eq!(rbc.read_bits_branchless(8), 0x88);
        let _ = rbc.reload();
        for expected in [0x77u32, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11] {
            assert_eq!(rbc.read_bits(8), Some(expected));
        }
        assert!(rbc.is_empty());

        // 2. SliceWriter batched u64_le_adv + into_rest.
        let mut buf = [0u8; 19];
        let mut sw = SliceWriter::new(&mut buf);
        assert!(sw.has_8());
        assert!(sw.write_u64_le_adv(u64::from_le_bytes(*b"abcdefgh"), 5));
        assert!(sw.write_u64_le_adv(u64::from_le_bytes(*b"12345678"), 6));
        let rest = sw.into_rest();
        assert_eq!(rest.len(), 8);
        rest.copy_from_slice(b"RESTTAIL");
        assert_eq!(&buf, b"abcde123456RESTTAIL");

        // 3. common_prefix_len across 8-byte chunks + tail.
        let hay = b"0123456789abcdefXYZ___0123456789abcdefX!!";
        assert_eq!(common_prefix_len(hay, 0, 22, hay.len()), 17);
        assert_eq!(common_prefix_len(hay, 0, 22, 30), 8);

        // 4. ByteCursor::exec_seq_within_cap with >16B literals & >16B match.
        let h = heap();
        let alloc = h.alloc();
        let mut out = Vec::try_with_capacity_in(512, alloc).expect("cap");
        let lits = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz__";
        {
            let mut cur = out.byte_cursor();
            assert!(cur.write_u64_le_adv(u64::from_le_bytes(*b"HEAD____"), 4));
            assert!(cur.exec_seq_within_cap(lits, 0, 24, 24, 24));
            assert!(cur.exec_seq_within_cap(lits, 24, 10, 20, 30));
            cur.finish();
        }
        assert_eq!(&out.as_slice()[..4], b"HEAD");
        assert_eq!(&out.as_slice()[4..28], &lits[..24]);
        assert_eq!(&out.as_slice()[28..52], &lits[..24]);
        assert_eq!(&out.as_slice()[52..62], &lits[24..34]);
        assert_eq!(out.len(), 92);
    }

    #[test]
    fn byte_cursor_leaf_batch_and_rev_bit_cursor_fast_reloads() {
        use super::{RevBitCursor, SeqOp};

        let h = heap();
        let alloc = h.alloc();
        let lits = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

        // 1. `try_exec_seq_leaf` covers <=16B & 17..=32B literals, >=16B & 8..15B offsets,
        //    and rejects out-of-contract inputs without mutating `len`.
        let mut out = Vec::try_with_capacity_in(512, alloc).expect("cap");
        let mut expected = Vec::new_in(alloc);
        {
            let mut cur = out.byte_cursor();
            assert!(cur.cap() >= 512);
            // 20B lits (>16, <=32) + 20B match (offset 20 >= 16, ml > 16)
            assert!(cur.try_exec_seq_leaf(lits, 0, 20, 20, 20));
            // 5B lits (<=16) + 40B match (offset 25 >= 16, ml > 32 wildcopy loop)
            assert!(cur.try_exec_seq_leaf(lits, 20, 5, 25, 40));
            // 18B lits + offset 10 (8..15 inline path), ml 13 (>8, <=16)
            assert!(cur.try_exec_seq_leaf(lits, 25, 18, 10, 13));
            // Rejections must leave `cur.len()` untouched:
            let saved_len = cur.len();
            assert!(!cur.try_exec_seq_leaf(lits, 43, 33, 16, 4)); // lit_len > 32
            assert!(!cur.try_exec_seq_leaf(lits, 43, 4, 4, 8)); // offset < 8
            assert!(!cur.try_exec_seq_leaf(lits, 43, 4, 10, 20)); // offset < 16 && ml > 16
            assert!(!cur.try_exec_seq_leaf(lits, 43, 4, 0, 4)); // offset == 0
            assert!(!cur.try_exec_seq_leaf(lits, 43, 4, saved_len + 100, 4)); // offset > after_lit
            assert!(!cur.try_exec_seq_leaf(lits, lits.len() - 10, 4, 16, 4)); // <32B lit slack
            assert_eq!(cur.len(), saved_len);
            cur.finish();
        }
        for &(lp, ll, off, ml) in &[(0usize, 20usize, 20usize, 20usize), (20, 5, 25, 40), (25, 18, 10, 13)]
        {
            expected.extend_from_slice(&lits[lp..lp + ll]);
            for _ in 0..ml {
                let b = expected[expected.len() - off];
                expected.push(b);
            }
        }
        assert_eq!(out.as_slice(), expected.as_slice());

        // 2. `exec_seq_batch` fast batch path + tail path.
        let mut batch_out = Vec::try_with_capacity_in(512, alloc).expect("cap");
        let seqs = [
            SeqOp { lit_len: 20, match_len: 20, offset: 20 },
            SeqOp { lit_len: 5, match_len: 40, offset: 25 },
            SeqOp { lit_len: 18, match_len: 13, offset: 10 },
        ];
        let mut lp = 0usize;
        {
            let mut cur = batch_out.byte_cursor();
            assert!(cur.exec_seq_batch(lits, &mut lp, &seqs));
            // Tail path (<32B literal slack remaining):
            let tail_start = lits.len() - 12;
            lp = tail_start;
            assert!(cur.exec_seq_batch(
                lits,
                &mut lp,
                &[SeqOp { lit_len: 12, match_len: 6, offset: 3 }],
            ));
            assert_eq!(lp, lits.len());
            cur.finish();
        }
        expected.extend_from_slice(&lits[lits.len() - 12..]);
        for _ in 0..6 {
            let b = expected[expected.len() - 3];
            expected.push(b);
        }
        assert_eq!(batch_out.as_slice(), expected.as_slice());

        // 3. `RevBitCursor::reload_if_slack8`, `bits_window`, and `reload_fast`.
        let stream: [u8; 20] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10, 0x11, 0x12, 0x13, 0x80,
        ];
        let mut rbc = RevBitCursor::new(&stream).expect("valid");
        assert_eq!(rbc.avail_bytes(), 12);
        assert!(rbc.reload_if_slack8());
        // Sentinel `0x80` leaves 7 zero bits before 0x13, 0x12, 0x11, ...
        let w = rbc.bits_window();
        assert_eq!(w >> (64 - 7), 0);
        assert_eq!((w << 7) >> (64 - 8), 0x13);
        rbc.advance_fast(7 + 8 + 8 + 8 + 8); // 1 sentinel + 39 = 40 bits -> advances 5 bytes
        assert!(rbc.reload_if_slack8());
        assert_eq!(rbc.avail_bytes(), 7);
        assert!(!rbc.reload_if_slack8()); // avail == 7 (< 8)
        rbc.advance_fast(32); // advances 4 bytes via clamping `reload_fast` -> avail becomes 3
        rbc.reload_fast();
        assert_eq!(rbc.avail_bytes(), 3);
    }
}
