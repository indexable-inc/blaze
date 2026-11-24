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

    /// Commit the written length back into the underlying [`Vec<u8, A>`].
    #[inline(always)]
    pub fn finish(self) {
        drop(self);
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
}
