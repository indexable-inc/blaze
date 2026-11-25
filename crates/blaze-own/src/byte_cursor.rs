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

use crate::{AllocFailure, Int, Vec};

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

/// Outcome of [`ByteCursor::exec_seq_leaf_checked`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeqLeafResult {
    /// Sequence executed inline and `ByteCursor::len` advanced.
    Done,
    /// Valid sequence that needs general out-of-line handling (e.g., `lit_len > 32`,
    /// or final literal/output slack region; for [`ByteCursor::exec_seq_leaf_checked`] also
    /// `offset < 8` or `offset < 16 && match_len > 16`).
    Pending,
    /// `lit_pos + lit_len > literals.len()`.
    LitOverflow,
    /// `self.len + lit_len + match_len > block_out_limit`.
    OutOverflow,
    /// `offset == 0 || offset > max_off`.
    BadOffset,
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
        let len = self.len;
        let after_lit = len.saturating_add(lit_len);
        let next_out = after_lit.saturating_add(match_len);
        if lit_len > 32
            || literals.len() < 32
            || lit_pos > literals.len() - 32
            || self.cap < 48
            || next_out > self.cap - 48
            || offset == 0
            || offset > after_lit
        {
            return false;
        }
        if (lit_len | match_len) <= 16 && offset >= 16 {
            // SAFETY:
            // - `lit_pos + 32 <= literals.len()` and `len + 32 < self.cap`, so 16B literal
            //   wildcopy stays strictly in bounds and non-overlapping.
            // - `16 <= offset <= after_lit` and `next_out + 48 <= self.cap`, so 16B match
            //   wildcopy reads initialized history non-overlapping with `dst` and writes in bounds.
            unsafe {
                let ls = literals.as_ptr().add(lit_pos);
                let ld = self.base.add(len);
                ptr::write_unaligned(ld.cast::<u128>(), ptr::read_unaligned(ls.cast::<u128>()));
                let ms = self.base.add(after_lit - offset);
                let md = self.base.add(after_lit);
                ptr::write_unaligned(md.cast::<u128>(), ptr::read_unaligned(ms.cast::<u128>()));
            }
            self.len = next_out;
            return true;
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
                ptr::write_unaligned(ld.cast::<u128>(), ptr::read_unaligned(ls.cast::<u128>()));
                if lit_len > 16 {
                    ptr::write_unaligned(
                        ld.add(16).cast::<u128>(),
                        ptr::read_unaligned(ls.add(16).cast::<u128>()),
                    );
                }
                let ms = self.base.add(after_lit - offset);
                let md = self.base.add(after_lit);
                ptr::write_unaligned(md.cast::<u64>(), ptr::read_unaligned(ms.cast::<u64>()));
                if match_len > 8 {
                    ptr::write_unaligned(
                        md.add(8).cast::<u64>(),
                        ptr::read_unaligned(ms.add(8).cast::<u64>()),
                    );
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
            ptr::write_unaligned(ld.cast::<u128>(), ptr::read_unaligned(ls.cast::<u128>()));
            if lit_len > 16 {
                ptr::write_unaligned(
                    ld.add(16).cast::<u128>(),
                    ptr::read_unaligned(ls.add(16).cast::<u128>()),
                );
            }

            let ms = self.base.add(after_lit - offset);
            let md = self.base.add(after_lit);
            ptr::write_unaligned(md.cast::<u128>(), ptr::read_unaligned(ms.cast::<u128>()));
            if match_len > 16 {
                ptr::write_unaligned(
                    md.add(16).cast::<u128>(),
                    ptr::read_unaligned(ms.add(16).cast::<u128>()),
                );
                if match_len > 32 {
                    let mut sp = ms.add(32);
                    let mut dp = md.add(32);
                    let end = md.add(match_len);
                    while dp < end {
                        ptr::write_unaligned(
                            dp.cast::<u128>(),
                            ptr::read_unaligned(sp.cast::<u128>()),
                        );
                        sp = sp.add(16);
                        dp = dp.add(16);
                    }
                }
            }
        }
        self.len = next_out;
        true
    }

    /// Call-free inline fast path with unified single-check validation for fused Zstandard
    /// sequence execution.
    ///
    /// Verifies literal bounds, `block_out_limit`, capacity slack, and window-clamped `offset`
    /// bounds once up front, then executes inline using unaligned 128-bit (`u128`) and 64-bit
    /// (`u64`) wildcopies without placing any out-of-line call in the hot leaf loop.
    ///
    /// On any non-[`SeqLeafResult::Done`] outcome, `self.len` is left unchanged.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub fn exec_seq_leaf_checked(
        &mut self,
        literals: &[u8],
        lit_pos: usize,
        lit_len: usize,
        offset: usize,
        match_len: usize,
        block_out_limit: usize,
        frame_start: usize,
        win_usize: usize,
        full_window: bool,
    ) -> SeqLeafResult {
        let len = self.len;
        let after_lit = len.saturating_add(lit_len);
        let next_out = after_lit.saturating_add(match_len);
        let max_off = if full_window {
            after_lit
        } else {
            after_lit.saturating_sub(frame_start).min(win_usize)
        };
        if lit_len > 32
            || literals.len() < 32
            || lit_pos > literals.len() - 32
            || self.cap < 48
            || next_out > block_out_limit.min(self.cap - 48)
            || offset.wrapping_sub(1) >= max_off
        {
            let Some(lit_end) = lit_pos.checked_add(lit_len) else {
                return SeqLeafResult::LitOverflow;
            };
            if lit_end > literals.len() {
                return SeqLeafResult::LitOverflow;
            }
            let Some(exact_after_lit) = len.checked_add(lit_len) else {
                return SeqLeafResult::OutOverflow;
            };
            let Some(exact_next_out) = exact_after_lit.checked_add(match_len) else {
                return SeqLeafResult::OutOverflow;
            };
            if exact_next_out > block_out_limit {
                return SeqLeafResult::OutOverflow;
            }
            let exact_max_off = if full_window {
                exact_after_lit
            } else {
                exact_after_lit.saturating_sub(frame_start).min(win_usize)
            };
            if offset.wrapping_sub(1) >= exact_max_off {
                return SeqLeafResult::BadOffset;
            }
            return SeqLeafResult::Pending;
        }

        if (lit_len | match_len) <= 16 && offset >= 16 {
            // SAFETY:
            // - `literals.len() >= 32` and `lit_pos <= literals.len() - 32`, so `lit_pos + 16 <= literals.len()`.
            // - `self.cap >= 48` and `next_out <= self.cap - 48`, so `after_lit == len + lit_len`,
            //   `next_out == after_lit + match_len`, `len + 16 < self.cap`, and `after_lit + 16 < self.cap`.
            // - `16 <= offset <= max_off <= after_lit`, so `after_lit - offset` points to 16 initialized
            //   bytes strictly before `after_lit`.
            unsafe {
                let ls = literals.as_ptr().add(lit_pos);
                let ld = self.base.add(len);
                ptr::write_unaligned(ld.cast::<u128>(), ptr::read_unaligned(ls.cast::<u128>()));
                let ms = self.base.add(after_lit - offset);
                let md = self.base.add(after_lit);
                ptr::write_unaligned(md.cast::<u128>(), ptr::read_unaligned(ms.cast::<u128>()));
            }
            self.len = next_out;
            return SeqLeafResult::Done;
        }

        if offset < 16 {
            if offset < 8 || match_len > 16 {
                return SeqLeafResult::Pending;
            }
            // SAFETY:
            // - `lit_pos + 32 <= literals.len()` and `len + 32 < self.cap`, so up to 2x16B literal
            //   wildcopies stay strictly in bounds.
            // - `8 <= offset <= max_off <= after_lit`, `match_len <= 16`, and `after_lit + 16 < self.cap`,
            //   so the first 8B load reads initialized history before `after_lit`, and after storing
            //   `md..md + 8` the optional second 8B load at `ms + 8` also reads initialized bytes.
            unsafe {
                let ls = literals.as_ptr().add(lit_pos);
                let ld = self.base.add(len);
                ptr::write_unaligned(ld.cast::<u128>(), ptr::read_unaligned(ls.cast::<u128>()));
                if lit_len > 16 {
                    ptr::write_unaligned(
                        ld.add(16).cast::<u128>(),
                        ptr::read_unaligned(ls.add(16).cast::<u128>()),
                    );
                }
                let ms = self.base.add(after_lit - offset);
                let md = self.base.add(after_lit);
                ptr::write_unaligned(md.cast::<u64>(), ptr::read_unaligned(ms.cast::<u64>()));
                if match_len > 8 {
                    ptr::write_unaligned(
                        md.add(8).cast::<u64>(),
                        ptr::read_unaligned(ms.add(8).cast::<u64>()),
                    );
                }
            }
            self.len = next_out;
            return SeqLeafResult::Done;
        }

        // SAFETY:
        // - `lit_pos + 32 <= literals.len()` and `len + 32 < self.cap`, so 1x16B / 2x16B literal
        //   wildcopies stay strictly in bounds.
        // - `16 <= offset <= max_off <= after_lit` and `next_out + 48 <= self.cap`, so every 16B
        //   match wildcopy chunk reads initialized history strictly before its destination (`sp + 16 <= dp`)
        //   and writes strictly inside `self.base..self.base + self.cap` (`dp + 16 <= base + next_out + 15 < base + cap`).
        unsafe {
            let ls = literals.as_ptr().add(lit_pos);
            let ld = self.base.add(len);
            ptr::write_unaligned(ld.cast::<u128>(), ptr::read_unaligned(ls.cast::<u128>()));
            if lit_len > 16 {
                ptr::write_unaligned(
                    ld.add(16).cast::<u128>(),
                    ptr::read_unaligned(ls.add(16).cast::<u128>()),
                );
            }

            let ms = self.base.add(after_lit - offset);
            let md = self.base.add(after_lit);
            ptr::write_unaligned(md.cast::<u128>(), ptr::read_unaligned(ms.cast::<u128>()));
            if match_len > 16 {
                ptr::write_unaligned(
                    md.add(16).cast::<u128>(),
                    ptr::read_unaligned(ms.add(16).cast::<u128>()),
                );
                if match_len > 32 {
                    let mut sp = ms.add(32);
                    let mut dp = md.add(32);
                    let end = md.add(match_len);
                    while dp < end {
                        ptr::write_unaligned(
                            dp.cast::<u128>(),
                            ptr::read_unaligned(sp.cast::<u128>()),
                        );
                        sp = sp.add(16);
                        dp = dp.add(16);
                    }
                }
            }
        }
        self.len = next_out;
        SeqLeafResult::Done
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
            (sum_lits_u64.narrow::<usize>(), sum_out_u64.narrow::<usize>())
        else {
            return self.exec_seq_batch_tail(literals, lit_pos, seqs);
        };

        let lit_slack_ok = fits_fast && lit_total - lp >= sum_lits.saturating_add(32);
        let out_slack_ok = fits_fast && cap - len >= sum_out.saturating_add(48);
        if lit_slack_ok && out_slack_ok {
            for seq in seqs {
                let lit_len = seq.lit_len.widen_to::<usize>();
                let match_len = seq.match_len.widen_to::<usize>();
                let offset = seq.offset.widen_to::<usize>();

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
            let lit_len = seq.lit_len.widen_to::<usize>();
            let match_len = seq.match_len.widen_to::<usize>();
            let offset = seq.offset.widen_to::<usize>();

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

    /// Open a pointer-based, register-resident [`SeqExecCtx`] for fused Zstandard sequence
    /// decoding.
    ///
    /// Specializes window bounds via `WINDOW_FULL`:
    /// - `WINDOW_FULL == false`: bounds `offset <= after_lit - window_base` (1 subtract + 1
    ///   compare, zero `max_window` register or `min`). Callers should cap `block_out_limit` at
    ///   `frame_start + max_window` when the block straddles the window boundary.
    /// - `WINDOW_FULL == true`: verifies once that `self.len - frame_start >= max_window`, then
    ///   bounds `offset <= max_window` (1 compare, zero `window_base` register or `min`).
    ///
    /// Call [`SeqExecCtx::commit`] (or [`SeqExecCtx::finish`]) after the loop to commit the
    /// updated output length back to this cursor.
    #[inline(always)]
    #[must_use]
    pub fn seq_exec_ctx<'b, const WINDOW_FULL: bool>(
        &'b mut self,
        literals: &'b [u8],
        lit_pos: usize,
        block_out_limit: usize,
        frame_start: usize,
        max_window: usize,
    ) -> SeqExecCtx<'b, 'a, A, WINDOW_FULL> {
        let out_base = self.base;
        let cur_len = self.len;
        // SAFETY: `cur_len <= self.cap` (`ByteCursor` invariant).
        let out_ptr = unsafe { out_base.add(cur_len) };

        let clamped_frame = frame_start.min(cur_len);
        // SAFETY: `clamped_frame <= cur_len <= self.cap`.
        let window_base: *const u8 = unsafe { out_base.add(clamped_frame) };

        // For `WINDOW_FULL == true`, prove once that at least `max_window` initialized history
        // bytes exist after `clamped_frame` (hence also `max_window <= cur_len`), so every
        // `offset <= max_window` stays within initialized `out_base..after_lit`.
        let window_ok = !WINDOW_FULL || cur_len - clamped_frame >= max_window;
        let out_limit_idx = if window_ok && self.cap >= 48 {
            let safe_idx = block_out_limit.min(self.cap - 48);
            if cur_len <= safe_idx {
                // `safe_idx + 1 <= self.cap - 47 <= self.cap` (strictly in bounds); strict `<`
                // in `exec_leaf_fast` proves `next_out <= safe_idx` and forces bail even when
                // `lit_len + match_len == 0` if slack or window verification failed (`out_limit == out_ptr`).
                safe_idx + 1
            } else {
                cur_len
            }
        } else {
            cur_len
        };
        // SAFETY: `out_limit_idx <= self.cap` in all branches.
        let out_limit = unsafe { out_base.add(out_limit_idx) };

        let lit_base = literals.as_ptr();
        let lit_total = literals.len();
        let clamped_lit = lit_pos.min(lit_total);
        // SAFETY: `clamped_lit <= lit_total`.
        let lit_ptr = unsafe { lit_base.add(clamped_lit) };
        let lit_limit_idx = if lit_total >= 32 && clamped_lit <= lit_total - 32 {
            // `lit_total - 31 <= lit_total` (strictly in bounds); strict `<` in `exec_leaf_fast`
            // guarantees at least 32 readable bytes at `lit_ptr`.
            lit_total - 31
        } else {
            clamped_lit
        };
        // SAFETY: `lit_limit_idx <= lit_total` in all branches.
        let lit_limit = unsafe { lit_base.add(lit_limit_idx) };

        SeqExecCtx {
            cursor: self,
            out_base,
            out_ptr,
            out_limit,
            lit_base,
            lit_ptr,
            lit_limit,
            window_base,
            max_window,
            _lit: PhantomData,
        }
    }

    /// Commit the written length back into the underlying [`Vec<u8, A>`].
    #[inline(always)]
    pub fn finish(self) {
        drop(self);
    }
}

/// Copy 32 unaligned bytes as paired 16-byte loads followed by paired 16-byte stores
/// (`ldp q, q; stp q, q` on AArch64 in both `blaze` and LLVM).
#[inline(always)]
unsafe fn copy_32_unaligned_pair(src: *const u8, dst: *mut u8) {
    // SAFETY: Caller guarantees `src..src + 32` is readable and `dst..dst + 32` is writable.
    unsafe {
        let v0 = ptr::read_unaligned(src.cast::<u128>());
        let v1 = ptr::read_unaligned(src.add(16).cast::<u128>());
        ptr::write_unaligned(dst.cast::<u128>(), v0);
        ptr::write_unaligned(dst.add(16).cast::<u128>(), v1);
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

/// Pointer-based, register-resident sequence execution cursor for fused Zstandard decoding.
///
/// Holds raw write/read cursors (`out_ptr`, `lit_ptr`) and slack limits (`out_limit`, `lit_limit`)
/// plus one active window bound (`window_base` when `!WINDOW_FULL`, `max_window` when
/// `WINDOW_FULL`) so only 5 registers from this context are live inside the hot sequence loop.
/// Intentionally implements no [`Drop`] so hot leaf loops emit zero drop-glue landing pads; call
/// [`SeqExecCtx::commit`] or [`SeqExecCtx::finish`] on loop exit to commit the output length back
/// to the parent [`ByteCursor`].
pub struct SeqExecCtx<'b, 'a, A: Allocator, const WINDOW_FULL: bool> {
    cursor: &'b mut ByteCursor<'a, A>,
    out_base: *mut u8,
    out_ptr: *mut u8,
    out_limit: *mut u8,
    lit_base: *const u8,
    lit_ptr: *const u8,
    lit_limit: *const u8,
    window_base: *const u8,
    max_window: usize,
    _lit: PhantomData<&'b [u8]>,
}

impl<A: Allocator, const WINDOW_FULL: bool> SeqExecCtx<'_, '_, A, WINDOW_FULL> {
    /// Number of initialized bytes written so far (including bytes executed in this context).
    #[inline(always)]
    #[must_use]
    pub fn len(&self) -> usize {
        // SAFETY: `self.out_ptr >= self.out_base` within `self.cursor`'s buffer (struct invariant).
        unsafe { self.out_ptr.offset_from_unsigned(self.out_base) }
    }

    /// Returns `true` when zero bytes have been written so far.
    #[inline(always)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.out_ptr == self.out_base
    }

    /// Current byte offset within the `literals` slice passed to [`ByteCursor::seq_exec_ctx`].
    #[inline(always)]
    #[must_use]
    pub fn lit_pos(&self) -> usize {
        // SAFETY: `self.lit_ptr >= self.lit_base` within `literals` (struct invariant).
        unsafe { self.lit_ptr.offset_from_unsigned(self.lit_base) }
    }

    /// Commit the register-cached output length back to the parent [`ByteCursor`] without moving
    /// `self` by value, keeping SROA scalar promotion intact.
    #[inline(always)]
    pub fn commit(&mut self) {
        self.cursor.len = self.len();
    }

    /// Consume this context and commit the register-cached output length back to the parent
    /// [`ByteCursor`].
    #[inline(always)]
    pub fn finish(mut self) {
        self.commit();
    }

    /// Execute one decoded sequence inline using pointer-resident bounds and NEON wildcopies:
    /// - `lit_len <= 32`: unconditional 32-byte (`2x16B`) NEON wildcopy (zero `lit_len <= 16`
    ///   branch).
    /// - `lit_len > 32`: inline 32-byte-per-iteration (`2x16B`) NEON wildcopy loop whenever
    ///   `lit_len <= lit_limit - lit_ptr` (zero out-of-line `memcpy` calls, keeping callers true
    ///   call-free leaf functions).
    /// - Matches: unconditional 32-byte NEON wildcopy for `offset >= 32` (with stride-32 `2x16B`
    ///   loop for `match_len > 32`), 16B wildcopy loop for `16 <= offset < 32`, inline
    ///   8B/doubled-16B copy for `8 <= offset < 16`, inline 64-bit pattern splat + paired 2x8B
    ///   wildcopy for `offset in {1, 2, 4}`, and period-multiple seed + 8B wildcopy for
    ///   `offset in {3, 5, 6, 7}`.
    ///
    /// Returns `true` when executed inline (`self.out_ptr` advanced by `lit_len + match_len`,
    /// `self.lit_ptr` advanced by `lit_len`). Returns `false` without mutating `self.out_ptr`,
    /// `self.lit_ptr`, or output memory whenever `offset == 0`, literal/output slack is exhausted
    /// (including `lit_len > lit_limit - lit_ptr` for `lit_len > 32`), or `offset` exceeds the
    /// active window, letting the caller classify and execute the sequence outside the hot loop.
    #[inline(always)]
    pub fn exec_leaf_fast(&mut self, lit_len: u32, offset: u32, match_len: u32) -> bool {
        if self.lit_ptr >= self.lit_limit {
            return false;
        }
        // SAFETY: `self.out_ptr <= self.out_limit` within `self.cursor`'s buffer (struct invariant).
        let avail_out = unsafe { self.out_limit.offset_from_unsigned(self.out_ptr) };
        let total_u64 = u64::from(lit_len) + u64::from(match_len);
        if total_u64 >= avail_out.widen_to::<u64>() {
            return false;
        }

        let ll = lit_len.widen_to::<usize>();
        let ml = match_len.widen_to::<usize>();
        let off = offset.widen_to::<usize>();

        // SAFETY: `ll <= total_u64 < avail_out`, so `after_lit < self.out_limit <= out_cap_end`.
        let after_lit = unsafe { self.out_ptr.add(ll) };
        let max_off = if WINDOW_FULL {
            self.max_window
        } else {
            // SAFETY: `after_lit >= self.out_ptr >= self.window_base` within the same allocation.
            unsafe { after_lit.offset_from_unsigned(self.window_base) }
        };
        // Single unsigned compare rejects both `off == 0` (`usize::MAX >= max_off` since
        // `max_off <= isize::MAX`) and `off > max_off`.
        if off.wrapping_sub(1) >= max_off {
            return false;
        }

        if ll <= 32 {
            // SAFETY:
            // - `self.lit_ptr < self.lit_limit == lit_base + lit_total - 31` guarantees
            //   `self.lit_ptr..self.lit_ptr + 32` lies strictly inside `literals` (immutable
            //   borrow, non-overlapping with `self.out_ptr`).
            // - `total_u64 < avail_out` guarantees `self.out_ptr + ll + ml <= min(block_out_limit,
            //   cap - 48)`, so `self.out_ptr + 32 <= out_ptr + ll + ml + 48 <= cap_end`.
            // - Writes exact `ll <= 32` literal bytes into `self.out_ptr..after_lit`; any wild
            //   bytes sit at `after_lit..self.out_ptr + 32` and are never read by the match copy.
            unsafe {
                copy_32_unaligned_pair(self.lit_ptr, self.out_ptr);
            }
        } else {
            // SAFETY: `self.lit_ptr < self.lit_limit` within `literals` (checked above).
            let lit_slack = unsafe { self.lit_limit.offset_from_unsigned(self.lit_ptr) };
            if ll > lit_slack {
                return false;
            }
            // SAFETY:
            // - `self.lit_ptr < self.lit_limit` implies `self.lit_limit == lit_base + lit_total - 31`,
            //   and `ll <= lit_slack = lit_limit - lit_ptr` proves `self.lit_ptr + ll <= lits_end - 31`,
            //   i.e. `self.lit_ptr + ll + 31 <= lits_end`.
            // - Because `ll > 32`, `dp = self.out_ptr < after_lit` strictly holds on entry; in
            //   every iteration of the bottom-tested loop, `k = dp - self.out_ptr <= ll - 1`, so:
            //   * Source reads `[self.lit_ptr + k .. self.lit_ptr + k + 32]` and steps `sp` to
            //     `self.lit_ptr + k + 32 <= self.lit_ptr + ll + 31 <= lits_end` (in bounds).
            //   * Dest writes `[self.out_ptr + k .. self.out_ptr + k + 32]` and steps `dp` to
            //     `self.out_ptr + k + 32 <= self.out_ptr + ll + 31 <= self.out_ptr + ll + ml + 31
            //     <= cap_end - 17 < cap_end` (strictly in bounds; 2x16B stride-32 stores prevent
            //     LLVM LoopIdiomRecognize from lowering the loop to a `memcpy` call).
            // - All `ll` literal bytes in `self.out_ptr..after_lit` are initialized; any wild
            //   bytes sit strictly at or past `after_lit` and are never read by the match copy.
            unsafe {
                let mut sp = self.lit_ptr;
                let mut dp = self.out_ptr;
                loop {
                    copy_32_unaligned_pair(sp, dp);
                    sp = sp.add(32);
                    dp = dp.add(32);
                    if dp >= after_lit {
                        break;
                    }
                }
            }
        }

        // SAFETY:
        // - All early exits have passed; `self.lit_ptr + ll <= lits_end` (`<= lits_end` for
        //   `ll <= 32` and `<= self.lit_limit < lits_end` for `ll > 32`), and
        //   `end = after_lit + ml < self.out_limit <= out_base + cap - 47`.
        // - `out_base..after_lit` is initialized with the exact `ll` literal bytes (wild bytes
        //   from literal copy sit strictly at or past `after_lit`, never read because `off >= 1`).
        // - `total_u64 < avail_out` guarantees `end <= min(block_out_limit, cap - 48)`, leaving
        //   at least 48 writable spare bytes past `end` (`after_lit + 32 <= cap_end - 16`, even
        //   when `ml == 0`).
        // - `1 <= off <= max_off`:
        //   * When `!WINDOW_FULL`: `max_off = after_lit - window_base`, `window_base >= out_base`.
        //   * When `WINDOW_FULL`: constructor proved `initial_out_ptr - out_base >= max_window`,
        //     and `after_lit >= self.out_ptr >= initial_out_ptr`.
        //   Hence `msrc = after_lit.sub(off)` lies inside initialized `out_base..after_lit`:
        //   * `off >= 32`: `msrc + 32 <= after_lit`, so unconditional 32B match wildcopy reads
        //     only initialized history; any `ml > 32` bottom-tested stride-32 loop enters with
        //     `dp = after_lit + 32 < end`, maintains `dp - sp == off >= 32` (so `sp + 32 <= dp`,
        //     strictly non-overlapping and reading only initialized bytes), and writes
        //     `<= end + 31 <= cap_end - 17 < cap_end`.
        //   * `16 <= off < 32`: `msrc + 16 <= after_lit`, so the initial 16B copy and any `ml > 16`
        //     bottom-tested 16B wildcopy loop (`dp = after_lit + 16 < end` on entry) maintain
        //     `dp - sp == off >= 16` and write `<= end + 15`.
        //   * `8 <= off < 16`: two 8B copies initialize `after_lit..after_lit + 16` with
        //     `dp - sp == off >= 8`; for `ml > 16`, doubling to period `2 * off >= 16` enters the
        //     bottom-tested loop with `dp = after_lit + 16 < end` and
        //     `sp = msrc.add(16 - off) >= msrc + 1 >= out_base` (`dp - sp == 2 * off >= 16`),
        //     reading strictly initialized bytes and ending at `<= end + 15`.
        //   * `off in {1, 2, 4}`: `after_lit.sub(off)..after_lit` holds `off` initialized bytes;
        //     splatting them to `word: u64` repeats the exact period across 8 bytes in native
        //     endianness. The initial 8B store and any `ml > 8` bottom-tested paired 2x8B
        //     stride-16 loop (`dp = after_lit + 8 < end` on entry; preventing
        //     `LoopIdiomRecognize` from emitting `memset`/`memset_pattern16`) write up to
        //     `end + 15 <= cap_end - 33 < cap_end`.
        //   * `off in {3, 5, 6, 7}`: `step = if off == 3 { 9 } else { off << 1 }` is an exact
        //     multiple of `off` with `9 <= step <= 14`. Seeding `0..step` (`step >= 9 > 0`) reads
        //     only initialized `msrc + i < after_lit + i` and writes `after_lit..after_lit + step`
        //     (`<= end + 14 <= cap_end - 34`). For `ml > step`, the bottom-tested loop enters with
        //     `dp = after_lit + step < end`; each 8B copy from `dp.sub(step)` reads strictly
        //     already-written bytes (`step >= 9 > 8`) at an exact multiple of `off`, writing up
        //     to `end + 7 <= cap_end - 41 < cap_end`.
        // - Finally, `self.lit_ptr` and `self.out_ptr` advance to their exact post-sequence
        //   positions (`self.out_ptr = end < self.out_limit`, preserving `self.out_ptr <= self.out_limit`).
        unsafe {
            self.lit_ptr = self.lit_ptr.add(ll);
            let end = after_lit.add(ml);
            self.out_ptr = end;

            if off >= 32 {
                let msrc = after_lit.sub(off);
                copy_32_unaligned_pair(msrc, after_lit);
                if ml > 32 {
                    let mut sp = msrc.add(32);
                    let mut dp = after_lit.add(32);
                    loop {
                        copy_32_unaligned_pair(sp, dp);
                        sp = sp.add(32);
                        dp = dp.add(32);
                        if dp >= end {
                            break;
                        }
                    }
                }
            } else if off >= 16 {
                let msrc = after_lit.sub(off);
                let m0 = ptr::read_unaligned(msrc.cast::<u128>());
                ptr::write_unaligned(after_lit.cast::<u128>(), m0);
                if ml > 16 {
                    let mut sp = msrc.add(16);
                    let mut dp = after_lit.add(16);
                    loop {
                        let v = ptr::read_unaligned(sp.cast::<u128>());
                        ptr::write_unaligned(dp.cast::<u128>(), v);
                        sp = sp.add(16);
                        dp = dp.add(16);
                        if dp >= end {
                            break;
                        }
                    }
                }
            } else if off >= 8 {
                let msrc = after_lit.sub(off);
                let m0 = ptr::read_unaligned(msrc.cast::<u64>());
                ptr::write_unaligned(after_lit.cast::<u64>(), m0);
                if ml > 8 {
                    let m1 = ptr::read_unaligned(msrc.add(8).cast::<u64>());
                    ptr::write_unaligned(after_lit.add(8).cast::<u64>(), m1);
                    if ml > 16 {
                        let mut sp = msrc.add(16 - off);
                        let mut dp = after_lit.add(16);
                        loop {
                            let v = ptr::read_unaligned(sp.cast::<u128>());
                            ptr::write_unaligned(dp.cast::<u128>(), v);
                            sp = sp.add(16);
                            dp = dp.add(16);
                            if dp >= end {
                                break;
                            }
                        }
                    }
                }
            } else if off == 1 || off == 2 || off == 4 {
                let word = if off == 1 {
                    u64::from(*after_lit.sub(1)).wrapping_mul(0x0101_0101_0101_0101)
                } else if off == 2 {
                    u64::from(ptr::read_unaligned(after_lit.sub(2).cast::<u16>()))
                        .wrapping_mul(0x0001_0001_0001_0001)
                } else {
                    let q = u64::from(ptr::read_unaligned(after_lit.sub(4).cast::<u32>()));
                    q | (q << 32)
                };
                ptr::write_unaligned(after_lit.cast::<u64>(), word);
                if ml > 8 {
                    let mut dp = after_lit.add(8);
                    loop {
                        ptr::write_unaligned(dp.cast::<u64>(), word);
                        ptr::write_unaligned(dp.add(8).cast::<u64>(), word);
                        dp = dp.add(16);
                        if dp >= end {
                            break;
                        }
                    }
                }
            } else {
                let step = if off == 3 { 9usize } else { off << 1 };
                let msrc = after_lit.sub(off);
                let mut i = 0usize;
                loop {
                    *after_lit.add(i) = *msrc.add(i);
                    i += 1;
                    if i >= step {
                        break;
                    }
                }
                if ml > step {
                    let mut dp = after_lit.add(step);
                    loop {
                        let v = ptr::read_unaligned(dp.sub(step).cast::<u64>());
                        ptr::write_unaligned(dp.cast::<u64>(), v);
                        dp = dp.add(8);
                        if dp >= end {
                            break;
                        }
                    }
                }
            }
        }
        true
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
            let unused_high = (8 - n).truncate_to::<u32>();
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
        let adv = (self.bits_consumed >> 3).widen_to::<usize>();
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
        let adv = (self.bits_consumed >> 3).widen_to::<usize>();
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
            let adv = (self.bits_consumed >> 3).widen_to::<usize>();
            let step = adv.min(avail);
            // SAFETY: `avail > 0` implies original `len >= 8`, and `step <= avail = ptr - start`,
            // so `self.ptr.sub(step)..self.ptr.sub(step) + 8` lies strictly inside the slice.
            unsafe {
                self.ptr = self.ptr.sub(step);
                self.container = u64::from_le(ptr::read_unaligned(self.ptr.cast::<u64>()));
            }
            let step_bits = step.truncate_to::<u32>() << 3;
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
        shifted.truncate_to::<u16>().widen_to::<usize>()
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
        shifted.truncate_to::<u32>()
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
        Some(shifted.truncate_to::<u32>())
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
        shifted.truncate_to::<u16>().widen_to::<u32>()
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

    /// Returns `true` when at least 8 unread bytes remain below `self.ptr` and `bits_consumed < 64`.
    #[inline(always)]
    #[must_use]
    pub fn has_slack8(&self) -> bool {
        self.avail_bytes() >= 8 && self.bits_consumed < 64
    }

    /// Returns `true` when at least 16 unread bytes remain below `self.ptr` and
    /// `bits_consumed < 64`.
    #[inline(always)]
    #[must_use]
    pub fn has_slack16(&self) -> bool {
        self.avail_bytes() >= 16 && self.bits_consumed < 64
    }

    /// Open a 3-word register-resident [`RevBitSlack8`] cursor from `self`.
    ///
    /// Callers check [`Self::has_slack8`] (or [`Self::has_slack16`]) before entering a hot loop
    /// and call [`RevBitSlack8::commit_to`] once on exit to write the position and reloaded
    /// `container` back to `self`.
    #[inline(always)]
    #[must_use]
    pub fn open_slack8(&self) -> RevBitSlack8<'a> {
        let limit: *const u8 = if self.avail_bytes() >= 8 {
            // SAFETY: `self.avail_bytes() >= 8` proves `self.start + 8 <= self.ptr` within the
            // borrowed slice `'a`, so `self.start.add(8)` is strictly in bounds and strictly
            // below `usize::MAX` (since `self.ptr..self.ptr + 8` lies inside `'a`).
            unsafe { self.start.add(8) }
        } else {
            // Sentinel `usize::MAX`: every valid slice pointer satisfies `self.ptr < limit`, so
            // `reload_window_slack8` always returns `None` without mutating `self.ptr`.
            ptr::without_provenance(usize::MAX)
        };
        RevBitSlack8 {
            limit,
            ptr: self.ptr,
            bits_consumed: u64::from(self.bits_consumed),
            _marker: PhantomData,
        }
    }
}

/// Register-resident 3-word reverse bitstream cursor for hot loops with `>= 8` bytes of slack.
///
/// Omits the cached `container` word (reloading it once on [`Self::commit_to`]) and stores
/// `limit = start + 8` (or sentinel `usize::MAX` when opened without 8-byte slack) plus
/// `bits_consumed` as a native `u64`, keeping only 3 scalar registers `(limit, ptr,
/// bits_consumed)` live across the copy body with a single `ptr < limit` check per reload (zero
/// `sub`, zero temp register) and zero aggregate copies so SROA promotes every field to SSA
/// registers.
pub struct RevBitSlack8<'a> {
    limit: *const u8,
    ptr: *const u8,
    bits_consumed: u64,
    _marker: PhantomData<&'a [u8]>,
}

impl<'a> RevBitSlack8<'a> {
    /// Unread bytes strictly below `self.ptr` (`0` when opened without 8-byte slack).
    #[inline(always)]
    #[must_use]
    pub fn avail_bytes(&self) -> usize {
        if self.limit.addr() == usize::MAX {
            return 0;
        }
        // SAFETY: Non-sentinel `self.limit == orig_start + 8` and `self.ptr > orig_start` within
        // the same borrowed slice `'a` (struct invariant), so `self.limit.sub(8)` equals
        // `orig_start <= self.ptr`.
        unsafe { self.ptr.offset_from_unsigned(self.limit.sub(8)) }
    }

    /// Reload an MSB-aligned 64-bit bit window when `self.ptr >= self.limit` (`avail_bytes() >= 8`)
    /// and `bits_consumed < 64`, stepping `self.ptr` down by `bits_consumed >> 3` (`<= 7`) and
    /// reducing `bits_consumed` modulo 8 (`<= 7`, leaving `>= 57` live bits at the MSB).
    ///
    /// Returns `None` without mutating `self` if fewer than 8 unread bytes remain or
    /// `bits_consumed >= 64`.
    #[inline(always)]
    pub fn reload_window_slack8(&mut self) -> Option<u64> {
        if self.ptr < self.limit {
            return None;
        }
        if self.bits_consumed >= 64 {
            return None;
        }
        let adv = (self.bits_consumed >> 3).truncate_to::<usize>();
        // SAFETY: `self.ptr >= self.limit` rules out the `usize::MAX` sentinel and proves
        // `self.ptr >= orig_start + 8`; `adv = self.bits_consumed >> 3 <= 7 < 8`, so
        // `next_ptr = self.ptr.sub(adv) >= orig_start + 1 > orig_start` and
        // `next_ptr..next_ptr + 8` lies strictly within the borrowed slice `'a`.
        let container = unsafe {
            let next_ptr = self.ptr.sub(adv);
            self.ptr = next_ptr;
            u64::from_le(ptr::read_unaligned(next_ptr.cast::<u64>()))
        };
        let rem_bits = self.bits_consumed & 7;
        self.bits_consumed = rem_bits;
        Some(container << rem_bits)
    }

    /// Reload an MSB-aligned 64-bit bit window when `self.avail_bytes() >= 16` and
    /// `bits_consumed < 64`, stepping `self.ptr` down by `bits_consumed >> 3` (`<= 7`) and
    /// reducing `bits_consumed` modulo 8 (`<= 7`, leaving `>= 57` live bits at the MSB and
    /// `>= 9` unread bytes of slack below `self.ptr` for a mid-sequence reload).
    ///
    /// Returns `None` without mutating `self` if fewer than 16 unread bytes remain or
    /// `bits_consumed >= 64`.
    #[inline(always)]
    pub fn reload_window_slack16(&mut self) -> Option<u64> {
        if self.limit.addr() == usize::MAX {
            return None;
        }
        if self.bits_consumed >= 64 {
            return None;
        }
        // SAFETY: Non-sentinel `self.limit == orig_start + 8` implies `open_slack8` observed
        // `orig_ptr >= orig_start + 8` with `orig_ptr..orig_ptr + 8` inside `'a`, so the borrowed
        // slice has length `>= 16` and `self.limit.add(8) == orig_start + 16` is in bounds (or
        // one-past-end) of `'a`.
        let limit16 = unsafe { self.limit.add(8) };
        if self.ptr < limit16 {
            return None;
        }
        let adv = (self.bits_consumed >> 3).truncate_to::<usize>();
        // SAFETY: `self.ptr >= limit16 == orig_start + 16` and `adv = self.bits_consumed >> 3 <= 7`,
        // so `next_ptr = self.ptr.sub(adv) >= orig_start + 9 > orig_start` and
        // `next_ptr..next_ptr + 8` lies strictly within the borrowed slice `'a`.
        let container = unsafe {
            let next_ptr = self.ptr.sub(adv);
            self.ptr = next_ptr;
            u64::from_le(ptr::read_unaligned(next_ptr.cast::<u64>()))
        };
        let rem_bits = self.bits_consumed & 7;
        self.bits_consumed = rem_bits;
        Some(container << rem_bits)
    }

    /// Branchless advance of `n` bits after a verified [`Self::reload_window_slack8`].
    #[inline(always)]
    pub const fn advance(&mut self, n: u64) {
        self.bits_consumed = self.bits_consumed.wrapping_add(n);
    }

    /// Write the updated cursor position and reloaded 64-bit `container` back into `dst` using
    /// scalar stores (zero aggregate struct copy).
    #[inline(always)]
    pub fn commit_to(&self, dst: &mut RevBitCursor<'a>) {
        if self.limit.addr() != usize::MAX {
            // SAFETY: Non-sentinel `self.limit == orig_start + 8` with `orig_start < self.ptr`
            // and `self.ptr..self.ptr + 8` strictly within `'a` (preserved by
            // `reload_window_slack8`), so `self.limit.sub(8)` is `orig_start` and reading 8 bytes
            // at `self.ptr` is strictly in bounds.
            let (orig_start, container) = unsafe {
                (self.limit.sub(8), u64::from_le(ptr::read_unaligned(self.ptr.cast::<u64>())))
            };
            dst.start = orig_start;
            dst.ptr = self.ptr;
            dst.container = container;
            dst.bits_consumed = self.bits_consumed.saturate::<u32>();
        } else if ptr::eq(dst.ptr, self.ptr) {
            dst.bits_consumed = self.bits_consumed.saturate::<u32>();
        }
    }
}

#[cold]
#[inline(never)]
fn reload_tail_val(start: *const u8, avail: usize, bits_consumed: u32) -> (u64, u32) {
    let step = avail.truncate_to::<u32>();
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

    /// Store `word` in little-endian order and advance the cursor by `adv.min(8)`.
    ///
    /// Returns `false` without writing if fewer than 8 bytes remain, or returns `false` after
    /// clamping the advance to `8` if `adv > 8`. When callers already guard [`Self::has_8`] and
    /// ignore the return value (e.g. 4-stream Huffman decoding where `adv <= 8` by construction),
    /// this compiles to a branchless unaligned store + clamped pointer advance.
    #[inline(always)]
    pub fn write_u64_le_adv(&mut self, word: u64, adv: usize) -> bool {
        if self.rem() < 8 {
            return false;
        }
        let step = adv.min(8);
        // SAFETY: `self.rem() >= 8` guarantees `self.ptr..self.ptr + 8 <= self.end`, and
        // `step <= 8` ensures `self.ptr.add(step) <= self.end` (preserving `self.ptr <= self.end`).
        unsafe {
            ptr::write_unaligned(self.ptr.cast::<u64>(), word.to_le());
            self.ptr = self.ptr.add(step);
        }
        adv <= 8
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
                return len + (diff.trailing_zeros() >> 3).widen_to::<usize>();
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
    use crate::{Int, Vec};

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

        // 2. SliceWriter batched u64_le_adv + clamping + into_rest.
        let mut buf = [0u8; 19];
        let mut sw = SliceWriter::new(&mut buf);
        assert!(sw.has_8());
        assert!(sw.write_u64_le_adv(u64::from_le_bytes(*b"abcdefgh"), 5));
        assert!(sw.write_u64_le_adv(u64::from_le_bytes(*b"12345678"), 6));
        let rest = sw.into_rest();
        assert_eq!(rest.len(), 8);
        rest.copy_from_slice(b"RESTTAIL");
        assert_eq!(&buf, b"abcde123456RESTTAIL");

        // `write_u64_le_adv` with `adv > 8` clamps safely to 8 and returns `false`; `< 8` rem
        // leaves the remaining bytes untouched and returns `false`.
        let mut clamp_buf = [0u8; 10];
        let mut csw = SliceWriter::new(&mut clamp_buf);
        assert!(!csw.write_u64_le_adv(u64::from_le_bytes(*b"ABCDEFGH"), 12));
        assert_eq!(csw.rem(), 2);
        assert!(!csw.has_8());
        assert!(!csw.write_u64_le_adv(u64::from_le_bytes(*b"XXXXXXXX"), 1));
        assert_eq!(csw.rem(), 2);
        let crest = csw.into_rest();
        crest.copy_from_slice(b"IJ");
        assert_eq!(&clamp_buf, b"ABCDEFGHIJ");

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

    #[test]
    fn byte_cursor_exec_seq_leaf_checked_all_outcomes() {
        use super::SeqLeafResult;

        let h = heap();
        let alloc = h.alloc();
        let lits = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

        let mut out = Vec::try_with_capacity_in(512, alloc).expect("cap");
        let mut expected = Vec::new_in(alloc);
        {
            let mut cur = out.byte_cursor();
            let block_limit = 300;
            // 1. <=16B lits + <=16B match (u128 fast path)
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 0, 16, 16, 12, block_limit, 0, 1024, true),
                SeqLeafResult::Done
            );
            // 2. 17..=32B lits + >32B match (2x u128 + wildcopy loop)
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 16, 20, 20, 40, block_limit, 0, 1024, true),
                SeqLeafResult::Done
            );
            // 3. 8 <= offset < 16 with match_len <= 16 (u64 path)
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 36, 18, 10, 13, block_limit, 0, 1024, false),
                SeqLeafResult::Done
            );
            let saved = cur.len();

            // 4. Pending cases (valid sequence, needs out-of-line general handler)
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 54, 33, 16, 4, block_limit, 0, 1024, true),
                SeqLeafResult::Pending
            );
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 54, 4, 4, 8, block_limit, 0, 1024, true),
                SeqLeafResult::Pending
            );
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 54, 4, 10, 20, block_limit, 0, 1024, true),
                SeqLeafResult::Pending
            );
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, lits.len() - 10, 4, 16, 4, block_limit, 0, 1024, true),
                SeqLeafResult::Pending
            );
            assert_eq!(
                cur.exec_seq_leaf_checked(b"", 0, 0, 16, 4, block_limit, 0, 1024, true),
                SeqLeafResult::Pending
            );

            // 5. Typed error classifications
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, lits.len() - 4, 5, 16, 4, block_limit, 0, 1024, true),
                SeqLeafResult::LitOverflow
            );
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 54, 4, 16, block_limit - saved, block_limit, 0, 1024, true),
                SeqLeafResult::OutOverflow
            );
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 54, 4, 0, 4, block_limit, 0, 1024, true),
                SeqLeafResult::BadOffset
            );
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 54, 4, saved + 5, 4, block_limit, 0, 1024, true),
                SeqLeafResult::BadOffset
            );
            // Window / frame_start clamp when full_window == false
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 54, 4, 32, 4, block_limit, 0, 16, false),
                SeqLeafResult::BadOffset
            );
            assert_eq!(
                cur.exec_seq_leaf_checked(lits, 54, 4, 20, 4, block_limit, saved - 10, 1024, false),
                SeqLeafResult::BadOffset
            );
            assert_eq!(cur.len(), saved);
            cur.finish();
        }

        for &(lp, ll, off, ml) in &[(0usize, 16usize, 16usize, 12usize), (16, 20, 20, 40), (36, 18, 10, 13)]
        {
            expected.extend_from_slice(&lits[lp..lp + ll]);
            for _ in 0..ml {
                let b = expected[expected.len() - off];
                expected.push(b);
            }
        }
        assert_eq!(out.as_slice(), expected.as_slice());
    }

    #[test]
    fn seq_exec_ctx_all_outcomes_and_short_slices() {
        use super::RevBitCursor;

        let h = heap();
        let alloc = h.alloc();
        let lits = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

        let mut out = Vec::try_with_capacity_in(512, alloc).expect("cap");
        let mut expected = Vec::new_in(alloc);
        let block_limit = 350;
        let done_seqs: [(u32, u32, u32); 9] = [
            // 1. <=16B lits + <=16B match, offset >= 16
            (16, 16, 12),
            // 2. 17..=32B lits + 17..=32B match, offset >= 16
            (20, 20, 24),
            // 3. <=16B lits + >32B match (wildcopy loop), offset >= 32
            (5, 36, 49),
            // 4. 17..=32B lits + <=8B match, 8 <= offset < 16
            (18, 10, 7),
            // 5. <=16B lits + 9..=16B match, 8 <= offset < 16
            (4, 8, 16),
            // 6. 0B lit + 0B match with valid offset >= 32
            (0, 32, 0),
            // 7. 8 <= offset < 16 with match_len > 16 executed inline
            (4, 10, 20),
            // 8. Exact 32B lit + 32B match at offset 32
            (32, 32, 32),
            // 9. >32B lit (inline 2x16B wildcopy loop) + match at offset 24
            (41, 24, 15),
        ];

        {
            let mut cur = out.byte_cursor();
            let mut ctx = cur.seq_exec_ctx::<false>(lits, 0, block_limit, 0, 1024);
            assert!(ctx.is_empty());
            assert_eq!(ctx.lit_pos(), 0);

            for &(ll, off, ml) in &done_seqs {
                assert!(ctx.exec_leaf_fast(ll, off, ml));
            }
            let saved_len = ctx.len();
            let saved_lit = ctx.lit_pos();
            assert!(!ctx.is_empty());
            assert_eq!(saved_lit, 140);

            // Rejections must return `false` and leave `len` and `lit_pos` untouched.
            assert!(!ctx.exec_leaf_fast(33, 16, 4)); // lit_len > 32 exceeding remaining lit_slack (15)
            assert!(!ctx.exec_leaf_fast(4, 0, 4)); // offset == 0
            assert!(!ctx.exec_leaf_fast(4, (saved_len + 5).truncate_to::<u32>(), 4)); // offset > after_lit
            assert!(!ctx.exec_leaf_fast(4, 16, (block_limit - saved_len).truncate_to::<u32>())); // > block_limit
            assert!(!ctx.exec_leaf_fast(4, 16, u32::MAX)); // huge match_len
            assert_eq!(ctx.len(), saved_len);
            assert_eq!(ctx.lit_pos(), saved_lit);
            ctx.commit();
            assert_eq!(cur.len(), saved_len);

            // WINDOW_FULL == false with non-zero `frame_start` rejects offsets reaching before `frame_start`.
            {
                let mut fctx = cur.seq_exec_ctx::<false>(lits, 0, block_limit, saved_len - 10, 1024);
                assert!(!fctx.exec_leaf_fast(4, 15, 4)); // 15 > 10 + 4
                assert!(fctx.exec_leaf_fast(4, 14, 4)); // 14 == 10 + 4
                fctx.commit();
            }
            let after_frame = cur.len();
            assert_eq!(after_frame, saved_len + 8);

            // WINDOW_FULL == true verifies `len - frame_start >= max_window` once up front.
            {
                // Misused WINDOW_FULL == true (`after_frame < 10_000`) must always bail safely.
                let mut bad_full = cur.seq_exec_ctx::<true>(lits, 0, block_limit, 0, 10_000);
                assert!(!bad_full.exec_leaf_fast(0, 8, 0));
                assert!(!bad_full.exec_leaf_fast(4, 16, 4));
                bad_full.commit();
            }
            assert_eq!(cur.len(), after_frame);
            {
                // Valid WINDOW_FULL == true (`max_window == 24 <= after_frame`).
                let mut full_ctx = cur.seq_exec_ctx::<true>(lits, 0, block_limit, 0, 24);
                assert!(!full_ctx.exec_leaf_fast(4, 25, 4)); // offset > max_window
                assert!(!full_ctx.exec_leaf_fast(4, 0, 4)); // offset == 0
                assert!(full_ctx.exec_leaf_fast(4, 24, 6)); // offset == max_window
                full_ctx.finish();
            }
            let after_full = cur.len();
            assert_eq!(after_full, after_frame + 10);

            // Empty and short (< 32B) literal slices must never enter the wildcopy fast path.
            {
                let mut empty_ctx = cur.seq_exec_ctx::<false>(b"", 0, block_limit, 0, 1024);
                assert!(!empty_ctx.exec_leaf_fast(0, 16, 4));
                assert!(!empty_ctx.exec_leaf_fast(0, 16, 0));
                assert!(!empty_ctx.exec_leaf_fast(1, 16, 4));
                empty_ctx.finish();
            }
            {
                let short_lits = b"hello_world";
                let mut short_ctx = cur.seq_exec_ctx::<false>(short_lits, 0, block_limit, 0, 1024);
                assert!(!short_ctx.exec_leaf_fast(5, 16, 4));
                short_ctx.finish();
            }
            // Exact 32-byte literal slice: `lit_pos == 1` bails, `lit_pos == 0` succeeds once then bails.
            let lits32 = b"0123456789abcdef0123456789abcdef";
            {
                let mut exact32_bail = cur.seq_exec_ctx::<false>(lits32, 1, block_limit, 0, 1024);
                assert!(!exact32_bail.exec_leaf_fast(4, 16, 4));
                exact32_bail.finish();

                let mut exact32_ok = cur.seq_exec_ctx::<false>(lits32, 0, block_limit, 0, 1024);
                assert!(exact32_ok.exec_leaf_fast(4, 16, 4));
                assert_eq!(exact32_ok.lit_pos(), 4);
                assert!(!exact32_ok.exec_leaf_fast(4, 16, 4)); // now lit_pos == 4 >= lit_limit (1)
                exact32_ok.finish();
            }
            assert_eq!(cur.len(), after_full + 8);
            cur.finish();
        }

        let mut lp = 0usize;
        for &(ll, off, ml) in &done_seqs {
            let llu = ll.widen_to::<usize>();
            let offu = off.widen_to::<usize>();
            let mlu = ml.widen_to::<usize>();
            expected.extend_from_slice(&lits[lp..lp + llu]);
            lp += llu;
            for _ in 0..mlu {
                let b = expected[expected.len() - offu];
                expected.push(b);
            }
        }
        // Frame-start test seq: lits[0..4], off 14, ml 4
        expected.extend_from_slice(&lits[..4]);
        for _ in 0..4 {
            let b = expected[expected.len() - 14];
            expected.push(b);
        }
        // Full-window test seq: lits[0..4], off 24, ml 6
        expected.extend_from_slice(&lits[..4]);
        for _ in 0..6 {
            let b = expected[expected.len() - 24];
            expected.push(b);
        }
        // Exact32 test seq: b"0123", off 16, ml 4
        expected.extend_from_slice(b"0123");
        for _ in 0..4 {
            let b = expected[expected.len() - 16];
            expected.push(b);
        }
        assert_eq!(out.as_slice(), expected.as_slice());

        // Short output capacity (< 48B) and zero capacity must never enter the wildcopy fast path.
        let mut tiny = Vec::try_with_capacity_in(16, alloc).expect("cap");
        {
            let mut cur = tiny.byte_cursor();
            assert!(cur.cap() < 48);
            assert!(cur.try_push(b'x').is_ok());
            let mut ctx = cur.seq_exec_ctx::<false>(lits, 0, 16, 0, 1024);
            assert!(!ctx.exec_leaf_fast(4, 16, 4));
            assert!(!ctx.exec_leaf_fast(0, 8, 0));
            assert!(!ctx.exec_leaf_fast(4, 4, 4));
            assert_eq!(ctx.len(), 1);
            assert_eq!(ctx.lit_pos(), 0);
            ctx.finish();
            cur.finish();
        }
        assert_eq!(tiny.as_slice(), b"x");

        let mut zero_cap = Vec::new_in(alloc);
        {
            let mut cur = zero_cap.byte_cursor();
            let mut ctx = cur.seq_exec_ctx::<false>(lits, 0, 0, 0, 1024);
            assert!(!ctx.exec_leaf_fast(0, 8, 0));
            assert!(!ctx.exec_leaf_fast(0, 1, 0));
            assert_eq!(ctx.len(), 0);
            assert_eq!(ctx.lit_pos(), 0);
            ctx.finish();
            cur.finish();
        }
        assert!(zero_cap.is_empty());

        // RevBitSlack8 round-trip with commit_to and exact tail handoff.
        let stream: [u8; 20] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10, 0x11, 0x12, 0x13, 0x80,
        ];
        let mut rbc = RevBitCursor::new(&stream).expect("valid");
        assert!(rbc.has_slack8());
        assert!(!rbc.has_slack16()); // avail == 12
        let mut bf = rbc.open_slack8();
        assert_eq!(bf.avail_bytes(), 12);
        let w = bf.reload_window_slack8().expect("slack8");
        assert_eq!(w >> (64 - 7), 0);
        assert_eq!((w << 7) >> (64 - 8), 0x13);
        bf.advance(7 + 32); // consume 7 zero bits + 0x13, 0x12, 0x11, 0x10
        let w2 = bf.reload_window_slack8().expect("slack8_2");
        assert_eq!(bf.avail_bytes(), 7);
        assert_eq!(w2 >> (64 - 8), 0x0F);
        bf.advance(8);
        assert!(bf.reload_window_slack8().is_none()); // avail == 7 (< 8)
        bf.commit_to(&mut rbc);
        assert_eq!(rbc.avail_bytes(), 7);
        assert_eq!(rbc.read_bits(8), Some(0x0E));

        // Exact `avail == 8` boundary (16-byte stream): `ptr == limit` reloads once cleanly.
        let exact16: [u8; 16] = [
            0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xA0, 0xB0, 0xC0, 0xD0, 0xE0,
            0xF0, 0x80,
        ];
        let mut rbc16 = RevBitCursor::new(&exact16).expect("valid16");
        assert!(rbc16.has_slack8());
        let mut bf16 = rbc16.open_slack8();
        assert_eq!(bf16.avail_bytes(), 8);
        let w16 = bf16.reload_window_slack8().expect("exact8");
        assert_eq!(w16 >> (64 - 7), 0);
        assert_eq!((w16 << 7) >> (64 - 8), 0xF0);
        bf16.advance(7 + 8);
        // `ptr` is still at `start + 8` until next reload steps it by 2 bytes to `start + 6`.
        let w16_2 = bf16.reload_window_slack8().expect("exact8_step");
        assert_eq!(bf16.avail_bytes(), 6);
        assert_eq!(w16_2 >> (64 - 8), 0xE0);
        bf16.advance(8);
        assert!(bf16.reload_window_slack8().is_none());
        bf16.commit_to(&mut rbc16);
        assert_eq!(rbc16.avail_bytes(), 6);
        assert_eq!(rbc16.read_bits(8), Some(0xD0));

        // Opening `RevBitSlack8` without 8-byte slack (`< 8` bytes and `avail < 8`) safely no-ops.
        let tiny3: [u8; 3] = [0xAB, 0xCD, 0x80];
        let mut rbc3 = RevBitCursor::new(&tiny3).expect("valid3");
        assert!(!rbc3.has_slack8());
        let mut bf3 = rbc3.open_slack8();
        assert_eq!(bf3.avail_bytes(), 0);
        assert!(bf3.reload_window_slack8().is_none());
        assert!(bf3.reload_window_slack16().is_none());
        bf3.commit_to(&mut rbc3);
        assert_eq!(rbc3.read_bits(7), Some(0));
        assert_eq!(rbc3.read_bits(8), Some(0xCD));
        assert_eq!(rbc3.read_bits(8), Some(0xAB));
        assert!(rbc3.is_empty());

        // Exact `avail == 16` boundary (24-byte stream) for `reload_window_slack16`.
        let exact24: [u8; 24] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x80,
        ];
        let mut rbc24 = RevBitCursor::new(&exact24).expect("valid24");
        assert!(rbc24.has_slack16());
        let mut bf24 = rbc24.open_slack8();
        assert_eq!(bf24.avail_bytes(), 16);
        let w24 = bf24.reload_window_slack16().expect("exact16");
        assert_eq!(w24 >> (64 - 7), 0);
        assert_eq!((w24 << 7) >> (64 - 8), 0x17);
        bf24.advance(7 + 8);
        let w24_2 = bf24.reload_window_slack16().expect("exact16_step");
        assert_eq!(bf24.avail_bytes(), 14);
        assert_eq!(w24_2 >> (64 - 8), 0x16);
        bf24.advance(8);
        assert!(bf24.reload_window_slack16().is_none());
        bf24.commit_to(&mut rbc24);
        assert_eq!(rbc24.avail_bytes(), 14);
        assert_eq!(rbc24.read_bits(8), Some(0x15));
    }

    #[test]
    fn seq_exec_ctx_exhaustive_offsets_lits_matches_and_windows() {
        let h = heap();
        let alloc = h.alloc();
        // Distinct bytes (> 150B) so any misalignment, period error, or literal slack bug is caught.
        let lits = b"AbCdEfGhIjKlMnOpQrStUvWxYz0123456789!@#$%^&*()-_=+[]{}|;:',.<>/?~`abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!@#$%^&*()-_=+[]{}|;:',.<>/?~`";
        let lit_slack = lits.len() - 31 - 7;
        let lit_lens: [u32; 13] = [
            0,
            1,
            15,
            16,
            17,
            31,
            32,
            33,
            48,
            64,
            65,
            lit_slack.truncate_to::<u32>(),
            (lit_slack + 1).truncate_to::<u32>(),
        ];
        let match_lens: [u32; 11] = [0, 3, 4, 15, 16, 17, 31, 32, 33, 48, 100];

        // Seed history with 64 distinct bytes so all offsets 1..=40 are valid in both modes.
        let seed = &lits[..64];

        for offset in 1u32..=40 {
            for &lit_len in &lit_lens {
                for &match_len in &match_lens {
                    let ll_u = lit_len.widen_to::<usize>();
                    let off_u = offset.widen_to::<usize>();
                    let ml_u = match_len.widen_to::<usize>();

                    // 1. WINDOW_FULL == false (growing window)
                    let exact_out = seed.len() + ll_u + ml_u;
                    let cap = exact_out + 48; // exact minimum 48B slack boundary
                    let mut out_grow = Vec::try_with_capacity_in(cap, alloc).expect("cap");
                    out_grow.extend_from_slice(seed);
                    let (grow_ok, grow_len, grow_lit) = {
                        let mut cur = out_grow.byte_cursor();
                        let mut ctx = cur.seq_exec_ctx::<false>(lits, 7, exact_out, 0, 4096);
                        let ok = ctx.exec_leaf_fast(lit_len, offset, match_len);
                        let len = ctx.len();
                        let lp = ctx.lit_pos();
                        ctx.commit();
                        cur.finish();
                        (ok, len, lp)
                    };

                    // 2. WINDOW_FULL == true (full window of 40 bytes <= seed.len() == 64)
                    let mut out_full = Vec::try_with_capacity_in(cap, alloc).expect("cap");
                    out_full.extend_from_slice(seed);
                    let (full_ok, full_len, full_lit) = {
                        let mut cur = out_full.byte_cursor();
                        let mut ctx = cur.seq_exec_ctx::<true>(lits, 7, exact_out, 0, 40);
                        let ok = ctx.exec_leaf_fast(lit_len, offset, match_len);
                        let len = ctx.len();
                        let lp = ctx.lit_pos();
                        ctx.commit();
                        cur.finish();
                        (ok, len, lp)
                    };

                    if lit_len > 32 && ll_u > lit_slack {
                        assert!(
                            !grow_ok,
                            "grow should bail for off={offset}, ll={lit_len}, ml={match_len}"
                        );
                        assert!(
                            !full_ok,
                            "full should bail for off={offset}, ll={lit_len}, ml={match_len}"
                        );
                        assert_eq!(grow_len, seed.len());
                        assert_eq!(full_len, seed.len());
                        assert_eq!(grow_lit, 7);
                        assert_eq!(full_lit, 7);
                        assert_eq!(out_grow.as_slice(), seed);
                        assert_eq!(out_full.as_slice(), seed);
                        continue;
                    }

                    assert!(grow_ok, "grow failed for off={offset}, ll={lit_len}, ml={match_len}");
                    assert!(full_ok, "full failed for off={offset}, ll={lit_len}, ml={match_len}");
                    assert_eq!(grow_len, exact_out);
                    assert_eq!(full_len, exact_out);
                    assert_eq!(grow_lit, 7 + ll_u);
                    assert_eq!(full_lit, 7 + ll_u);

                    let mut expected = Vec::new_in(alloc);
                    expected.extend_from_slice(seed);
                    expected.extend_from_slice(&lits[7..7 + ll_u]);
                    for _ in 0..ml_u {
                        let b = expected[expected.len() - off_u];
                        expected.push(b);
                    }
                    assert_eq!(
                        out_grow.as_slice(),
                        expected.as_slice(),
                        "grow mismatch for off={offset}, ll={lit_len}, ml={match_len}"
                    );
                    assert_eq!(
                        out_full.as_slice(),
                        expected.as_slice(),
                        "full mismatch for off={offset}, ll={lit_len}, ml={match_len}"
                    );
                }
            }
        }

        // Also test cold start (`after_lit == offset`) for all offsets 1..=40 (including 1..=7
        // overlapping short offsets and `lit_len > 32`) inline at the exact 48B slack boundary.
        for offset in 1u32..=40 {
            let off_u = offset.widen_to::<usize>();
            for &match_len in &match_lens {
                let ml_u = match_len.widen_to::<usize>();
                let exact_out = off_u + ml_u;
                let mut out_cold = Vec::try_with_capacity_in(exact_out + 48, alloc).expect("cap");
                let (cold_ok, cold_len, cold_lit) = {
                    let mut cur = out_cold.byte_cursor();
                    let mut ctx = cur.seq_exec_ctx::<false>(lits, 0, exact_out, 0, 4096);
                    let ok = ctx.exec_leaf_fast(offset, offset, match_len);
                    let len = ctx.len();
                    let lp = ctx.lit_pos();
                    ctx.commit();
                    cur.finish();
                    (ok, len, lp)
                };
                assert!(cold_ok, "cold failed for off={offset}, ml={match_len}");
                assert_eq!(cold_len, exact_out);
                assert_eq!(cold_lit, off_u);
                let mut expected = Vec::new_in(alloc);
                expected.extend_from_slice(&lits[..off_u]);
                for _ in 0..ml_u {
                    let b = expected[expected.len() - off_u];
                    expected.push(b);
                }
                assert_eq!(
                    out_cold.as_slice(),
                    expected.as_slice(),
                    "cold mismatch for off={offset}, ml={match_len}"
                );
            }
        }
    }

    #[test]
    fn seq_exec_ctx_all_offsets_1_to_36_dense_oracle() {
        let h = heap();
        let alloc = h.alloc();

        let mut seed = [0u8; 64];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = i.wrapping_mul(37).wrapping_add(13).truncate_to::<u8>();
        }
        let mut lits = [0u8; 128];
        for (i, b) in lits.iter_mut().enumerate() {
            *b = i.wrapping_mul(53).wrapping_add(7).truncate_to::<u8>();
        }

        let lit_lens: [u32; 5] = [0, 1, 7, 32, 40];
        let mut out = Vec::try_with_capacity_in(256, alloc).expect("cap");
        let mut expected = Vec::try_with_capacity_in(256, alloc).expect("cap");

        for offset in 1u32..=36 {
            let off_u = offset.widen_to::<usize>();
            for &lit_len in &lit_lens {
                let ll_u = lit_len.widen_to::<usize>();
                for match_len in 3u32..=80 {
                    let ml_u = match_len.widen_to::<usize>();
                    let exact_out = seed.len() + ll_u + ml_u;

                    out.clear();
                    out.extend_from_slice(&seed);
                    let (ok, len, lp) = {
                        let mut cur = out.byte_cursor();
                        let mut ctx = cur.seq_exec_ctx::<false>(&lits, 0, exact_out, 0, 4096);
                        let ok = ctx.exec_leaf_fast(lit_len, offset, match_len);
                        let len = ctx.len();
                        let lp = ctx.lit_pos();
                        ctx.commit();
                        cur.finish();
                        (ok, len, lp)
                    };
                    assert!(ok, "failed for off={offset}, ll={lit_len}, ml={match_len}");
                    assert_eq!(len, exact_out);
                    assert_eq!(lp, ll_u);

                    expected.clear();
                    expected.extend_from_slice(&seed);
                    expected.extend_from_slice(&lits[..ll_u]);
                    for _ in 0..ml_u {
                        let b = expected[expected.len() - off_u];
                        expected.push(b);
                    }
                    assert_eq!(
                        out.as_slice(),
                        expected.as_slice(),
                        "mismatch for off={offset}, ll={lit_len}, ml={match_len}"
                    );
                }
            }
        }

        // Exact +48B capacity slack boundary for all short offsets 1..=7 across match lengths 0..=80
        // in both WINDOW_FULL = false and WINDOW_FULL = true, plus offset == 0 rejection.
        for offset in 1u32..=7 {
            let off_u = offset.widen_to::<usize>();
            for match_len in 0u32..=80 {
                let ml_u = match_len.widen_to::<usize>();
                let exact_out = seed.len() + 7 + ml_u;
                let cap = exact_out + 48;

                let mut out_grow = Vec::try_with_capacity_in(cap, alloc).expect("cap");
                out_grow.extend_from_slice(&seed);
                {
                    let mut cur = out_grow.byte_cursor();
                    let mut ctx = cur.seq_exec_ctx::<false>(&lits, 0, exact_out, 0, 4096);
                    assert!(!ctx.exec_leaf_fast(7, 0, match_len));
                    assert!(ctx.exec_leaf_fast(7, offset, match_len));
                    assert_eq!(ctx.len(), exact_out);
                    assert_eq!(ctx.lit_pos(), 7);
                    ctx.commit();
                    cur.finish();
                }

                let mut out_full = Vec::try_with_capacity_in(cap, alloc).expect("cap");
                out_full.extend_from_slice(&seed);
                {
                    let mut cur = out_full.byte_cursor();
                    let mut ctx = cur.seq_exec_ctx::<true>(&lits, 0, exact_out, 0, 32);
                    assert!(!ctx.exec_leaf_fast(7, 0, match_len));
                    assert!(ctx.exec_leaf_fast(7, offset, match_len));
                    assert_eq!(ctx.len(), exact_out);
                    assert_eq!(ctx.lit_pos(), 7);
                    ctx.commit();
                    cur.finish();
                }

                expected.clear();
                expected.extend_from_slice(&seed);
                expected.extend_from_slice(&lits[..7]);
                for _ in 0..ml_u {
                    let b = expected[expected.len() - off_u];
                    expected.push(b);
                }
                assert_eq!(
                    out_grow.as_slice(),
                    expected.as_slice(),
                    "exact-slack grow mismatch for off={offset}, ml={match_len}"
                );
                assert_eq!(
                    out_full.as_slice(),
                    expected.as_slice(),
                    "exact-slack full mismatch for off={offset}, ml={match_len}"
                );
            }
        }
    }
}
