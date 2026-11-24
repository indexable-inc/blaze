//! Zero-bounds-check primitives for `bstd` hot loops:
//!
//! - [`low_u32`] and [`top_bits`]: infallible bit-slice helpers with zero `as` casts and zero
//!   `try_from` panic paths.
//! - [`Pow2Table`]: a heap-allocated power-of-two lookup table whose length is invariantly
//!   `1 << LOG`, eliminating bounds checks on top-bit and masked accesses.
//! - [`SlackSlice`]: a borrowed byte slice paired with an explicit `SLACK` margin so unaligned
//!   little-endian reads at `pos <= self.limit()` are proven in-bounds.

use core::alloc::Allocator;
use core::ptr;

use crate::{AllocFailure, Vec};

/// Extract the low 32 bits of `v` without `as` casts or fallible conversions.
///
/// Compiles to zero instructions on 64-bit targets.
#[inline(always)]
#[must_use]
pub const fn low_u32(v: u64) -> u32 {
    let [b0, b1, b2, b3, ..] = v.to_le_bytes();
    u32::from_le_bytes([b0, b1, b2, b3])
}

#[cfg(target_pointer_width = "64")]
#[inline(always)]
const fn u32_to_usize(v: u32) -> usize {
    let [b0, b1, b2, b3] = v.to_le_bytes();
    usize::from_le_bytes([b0, b1, b2, b3, 0, 0, 0, 0])
}

#[cfg(target_pointer_width = "32")]
#[inline(always)]
const fn u32_to_usize(v: u32) -> usize {
    usize::from_le_bytes(v.to_le_bytes())
}

/// Extract the top `LOG` bits (`1 <= LOG <= 30`) of `hash` as a `usize` in `0..(1 << LOG)`.
///
/// Uses zero `as` casts and zero panic paths; compiles to a single logical right shift.
#[inline(always)]
#[must_use]
pub const fn top_bits<const LOG: u32>(hash: u64) -> usize {
    const {
        assert!(LOG >= 1 && LOG <= 30, "top_bits requires 1 <= LOG <= 30");
    }
    u32_to_usize(low_u32(hash >> (64 - LOG)))
}

/// A fixed-size power-of-two lookup table whose length is invariantly `1 << LOG`.
///
/// Because `self.data.len() == 1 << LOG` is established at construction and never resized,
/// indexing by [`top_bits::<LOG>`] or `idx & ((1 << LOG) - 1)` is unconditionally in bounds
/// with zero runtime bounds checks.
pub struct Pow2Table<T, const LOG: u32, A: Allocator> {
    data: Vec<T, A>,
}

impl<T, const LOG: u32, A: Allocator> Pow2Table<T, LOG, A> {
    /// Number of entries in the table (`1 << LOG`).
    pub const LEN: usize = {
        assert!(LOG >= 1 && LOG <= 24, "Pow2Table requires 1 <= LOG <= 24");
        1usize << LOG
    };

    /// Bitmask for wrapping an arbitrary `usize` index into `0..LEN`.
    pub const MASK: usize = Self::LEN - 1;

    /// Allocate a table of `1 << LOG` entries initialized to `val` in `alloc`.
    pub fn try_new_filled(val: T, alloc: A) -> Result<Self, AllocFailure>
    where
        T: Copy,
    {
        const {
            assert!(LOG >= 1 && LOG <= 24, "Pow2Table requires 1 <= LOG <= 24");
        }
        let len = Self::LEN;
        let mut data = Vec::try_with_capacity_in(len, alloc)?;
        for slot in data.spare_capacity_mut().iter_mut().take(len) {
            slot.write(val);
        }
        // SAFETY: `try_with_capacity_in(len, alloc)` guaranteed `capacity() >= len`,
        // `data.len()` was 0 so `spare_capacity_mut().len() >= len`, and the loop above
        // initialized every slot in `0..len`.
        unsafe {
            data.set_len(len);
        }
        Ok(Self { data })
    }

    /// Number of entries in the table (`1 << LOG`).
    #[inline(always)]
    #[must_use]
    pub const fn len(&self) -> usize {
        Self::LEN
    }

    /// Always `false` because `LOG >= 1` implies `LEN >= 2`.
    #[inline(always)]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// View the underlying table as a slice of length `1 << LOG`.
    #[inline(always)]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        self.data.as_slice()
    }

    /// View the underlying table as a mutable slice of length `1 << LOG`.
    #[inline(always)]
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.data.as_mut_slice()
    }

    /// Overwrite every entry in the table with `val`.
    #[inline]
    pub fn fill(&mut self, val: T)
    where
        T: Copy,
    {
        self.data.as_mut_slice().fill(val);
    }

    /// Read the entry indexed by the top `LOG` bits of `hash` with zero bounds check.
    #[inline(always)]
    #[must_use]
    pub fn get_top(&self, hash: u64) -> T
    where
        T: Copy,
    {
        let idx = top_bits::<LOG>(hash);
        // SAFETY: `Pow2Table` maintains `self.data.len() == 1 << LOG`, and
        // `top_bits::<LOG>(hash)` shifts right by `64 - LOG`, guaranteeing `idx < 1 << LOG`.
        unsafe { *self.data.as_ptr().add(idx) }
    }

    /// Replace the entry indexed by the top `LOG` bits of `hash` with `new_val`,
    /// returning the previous value, with zero bounds check.
    #[inline(always)]
    pub fn swap_top(&mut self, hash: u64, new_val: T) -> T
    where
        T: Copy,
    {
        let idx = top_bits::<LOG>(hash);
        // SAFETY: `Pow2Table` maintains `self.data.len() == 1 << LOG`, and
        // `top_bits::<LOG>(hash)` guarantees `idx < 1 << LOG`. `&mut self` grants
        // exclusive access to the initialized slot.
        unsafe {
            let slot = self.data.as_mut_ptr().add(idx);
            let prev = *slot;
            *slot = new_val;
            prev
        }
    }

    /// Read the entry at `idx & ((1 << LOG) - 1)` with zero bounds check.
    #[inline(always)]
    #[must_use]
    pub fn get_masked(&self, idx: usize) -> T
    where
        T: Copy,
    {
        let masked = idx & Self::MASK;
        // SAFETY: `Pow2Table` maintains `self.data.len() == 1 << LOG`, and
        // `Self::MASK == (1 << LOG) - 1`, guaranteeing `masked < 1 << LOG`.
        unsafe { *self.data.as_ptr().add(masked) }
    }

    /// Write `val` to the entry at `idx & ((1 << LOG) - 1)` with zero bounds check.
    #[inline(always)]
    pub fn set_masked(&mut self, idx: usize, val: T) {
        let masked = idx & Self::MASK;
        // SAFETY: `Pow2Table` maintains `self.data.len() == 1 << LOG`, and
        // `Self::MASK == (1 << LOG) - 1`, guaranteeing `masked < 1 << LOG`.
        // `&mut self` grants exclusive access to the initialized slot.
        unsafe {
            *self.data.as_mut_ptr().add(masked) = val;
        }
    }
}

/// A borrowed byte slice paired with an explicit `SLACK` margin (`limit = len.saturating_sub(SLACK)`).
///
/// Whenever `self.len() >= SLACK`, any offset `pos <= self.limit()` is statically guaranteed
/// to have at least `SLACK` readable bytes ahead in `self.as_slice()`.
#[derive(Clone, Copy, Debug)]
pub struct SlackSlice<'a, const SLACK: usize> {
    bytes: &'a [u8],
    limit: usize,
}

impl<'a, const SLACK: usize> SlackSlice<'a, SLACK> {
    /// Wrap `bytes` with an explicit readable-tail margin of `SLACK` bytes.
    #[inline(always)]
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, limit: bytes.len().saturating_sub(SLACK) }
    }

    /// `self.len().saturating_sub(SLACK)`.
    #[inline(always)]
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Total length of the wrapped slice in bytes.
    #[inline(always)]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns `true` when the wrapped slice has zero bytes.
    #[inline(always)]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Borrow the underlying byte slice.
    #[inline(always)]
    #[must_use]
    pub const fn as_slice(&self) -> &'a [u8] {
        self.bytes
    }

    /// Read an unaligned little-endian `u32` at `pos` when `SLACK` bytes remain ahead.
    ///
    /// Returns `None` if `self.len() < SLACK` or `pos > self.limit()`.
    #[inline(always)]
    #[must_use]
    pub fn read_u32_le(&self, pos: usize) -> Option<u32> {
        const {
            assert!(SLACK >= 4, "SlackSlice::read_u32_le requires SLACK >= 4");
        }
        if self.bytes.len() < SLACK || pos > self.limit {
            return None;
        }
        // SAFETY: `self.bytes.len() >= SLACK >= 4` and
        // `pos <= self.limit == self.bytes.len() - SLACK`, so
        // `pos + 4 <= pos + SLACK <= self.bytes.len()`. Reading 4 unaligned bytes at
        // `self.bytes.as_ptr().add(pos)` stays within the borrowed slice.
        let v = unsafe { ptr::read_unaligned(self.bytes.as_ptr().add(pos).cast::<u32>()) };
        Some(u32::from_le(v))
    }

    /// Read an unaligned little-endian `u64` at `pos` when `SLACK` bytes remain ahead.
    ///
    /// Returns `None` if `self.len() < SLACK` or `pos > self.limit()`.
    #[inline(always)]
    #[must_use]
    pub fn read_u64_le(&self, pos: usize) -> Option<u64> {
        const {
            assert!(SLACK >= 8, "SlackSlice::read_u64_le requires SLACK >= 8");
        }
        if self.bytes.len() < SLACK || pos > self.limit {
            return None;
        }
        // SAFETY: `self.bytes.len() >= SLACK >= 8` and
        // `pos <= self.limit == self.bytes.len() - SLACK`, so
        // `pos + 8 <= pos + SLACK <= self.bytes.len()`. Reading 8 unaligned bytes at
        // `self.bytes.as_ptr().add(pos)` stays within the borrowed slice.
        let v = unsafe { ptr::read_unaligned(self.bytes.as_ptr().add(pos).cast::<u64>()) };
        Some(u64::from_le(v))
    }
}

#[cfg(test)]
mod tests {
    use super::{Pow2Table, SlackSlice, low_u32, top_bits};
    use crate::testing::heap;

    #[test]
    fn low_u32_extracts_lower_half() {
        assert_eq!(low_u32(0), 0);
        assert_eq!(low_u32(0x1234_5678_9ABC_DEF0), 0x9ABC_DEF0);
        assert_eq!(low_u32(0xFFFF_FFFF_0000_0000), 0);
        assert_eq!(low_u32(0x0000_0000_DEAD_BEEF), 0xDEAD_BEEF);
        assert_eq!(low_u32(u64::MAX), u32::MAX);
    }

    #[test]
    fn top_bits_extracts_high_bits() {
        assert_eq!(top_bits::<1>(0), 0);
        assert_eq!(top_bits::<1>(1u64 << 63), 1);
        assert_eq!(top_bits::<1>((1u64 << 63) - 1), 0);
        assert_eq!(top_bits::<8>(0xAB00_0000_0000_0000), 0xAB);
        assert_eq!(top_bits::<12>(0xF0E0_1234_5678_9ABC), 0xF0E);
        assert_eq!(top_bits::<30>(u64::MAX), (1usize << 30) - 1);
        assert_eq!(top_bits::<30>(1u64 << 34), 1);
        assert_eq!(top_bits::<30>((1u64 << 34) - 1), 0);
    }

    #[test]
    fn pow2_table_ops_and_fill() {
        let h = heap();
        let alloc = h.alloc();
        let mut tbl: Pow2Table<u32, 6, _> = Pow2Table::try_new_filled(0xCAFE, alloc).expect("alloc");
        assert_eq!(tbl.len(), 64);
        assert!(!tbl.is_empty());

        // Every slot starts at the initial fill value.
        for i in 0..64 {
            assert_eq!(tbl.get_masked(i), 0xCAFE);
        }

        // `swap_top` / `get_top` index by the top 6 bits.
        let h_slot_42: u64 = 42u64 << (64 - 6);
        let h_slot_42_noise: u64 = h_slot_42 | 0x03FF_FFFF_FFFF_FFFF;
        assert_eq!(tbl.swap_top(h_slot_42, 777), 0xCAFE);
        assert_eq!(tbl.get_top(h_slot_42_noise), 777);
        assert_eq!(tbl.get_masked(42), 777);
        assert_eq!(tbl.swap_top(h_slot_42_noise, 888), 777);
        assert_eq!(tbl.get_masked(42 + 64), 888);

        // `set_masked` / `get_masked` wrap indices modulo 64.
        tbl.set_masked(64 + 7, 1234);
        assert_eq!(tbl.get_masked(7), 1234);
        assert_eq!(tbl.get_masked(128 + 7), 1234);
        tbl.set_masked(usize::MAX, 9999);
        assert_eq!(tbl.get_masked(63), 9999);

        // `fill` resets all entries.
        tbl.fill(0);
        for i in 0..64 {
            assert_eq!(tbl.get_masked(i), 0);
        }
        assert_eq!(tbl.get_top(h_slot_42), 0);
    }

    #[test]
    fn slack_slice_in_bounds_and_boundary_none() {
        let data: [u8; 12] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C];

        // SLACK = 8: limit = 12 - 8 = 4.
        let s8 = SlackSlice::<8>::new(&data);
        assert_eq!(s8.len(), 12);
        assert!(!s8.is_empty());
        assert_eq!(s8.limit(), 4);
        assert_eq!(s8.as_slice(), &data);

        assert_eq!(s8.read_u32_le(0), Some(0x0403_0201));
        assert_eq!(s8.read_u32_le(4), Some(0x0807_0605));
        assert_eq!(s8.read_u32_le(5), None);
        assert_eq!(s8.read_u32_le(usize::MAX), None);

        assert_eq!(s8.read_u64_le(0), Some(0x0807_0605_0403_0201));
        assert_eq!(s8.read_u64_le(4), Some(0x0C0B_0A09_0807_0605));
        assert_eq!(s8.read_u64_le(5), None);
        assert_eq!(s8.read_u64_le(usize::MAX), None);

        // Exact length == SLACK: pos 0 succeeds, pos 1 is None.
        let exact = SlackSlice::<8>::new(&data[..8]);
        assert_eq!(exact.limit(), 0);
        assert_eq!(exact.read_u64_le(0), Some(0x0807_0605_0403_0201));
        assert_eq!(exact.read_u64_le(1), None);
        assert_eq!(exact.read_u32_le(0), Some(0x0403_0201));
        assert_eq!(exact.read_u32_le(1), None);

        // Shorter than SLACK (including empty): pos 0 must return None, never over-read.
        let short = SlackSlice::<8>::new(&data[..7]);
        assert_eq!(short.limit(), 0);
        assert_eq!(short.read_u64_le(0), None);
        assert_eq!(short.read_u32_le(0), None);

        let empty = SlackSlice::<4>::new(&[]);
        assert!(empty.is_empty());
        assert_eq!(empty.limit(), 0);
        assert_eq!(empty.read_u32_le(0), None);

        // SLACK = 4 boundary on a 5-byte slice.
        let s4 = SlackSlice::<4>::new(&data[..5]);
        assert_eq!(s4.limit(), 1);
        assert_eq!(s4.read_u32_le(0), Some(0x0403_0201));
        assert_eq!(s4.read_u32_le(1), Some(0x0504_0302));
        assert_eq!(s4.read_u32_le(2), None);
    }
}
