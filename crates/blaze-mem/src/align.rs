//! Rounding to a power-of-two alignment, for `usize`, `u32` and `u64`.
//!
//! For an alignment known only at run time (a `Layout`'s, a section's, a page size read from
//! the kernel), std's `next_multiple_of` and `is_multiple_of` divide, which takes tens of
//! cycles, while a power of two needs one add and one mask. To round a value up to a constant,
//! `next_multiple_of` is as fast (the compiler makes the same mask) and needs no import; std
//! has no rounding down, so `align_down` and `align_range` serve constants too.
//!
//! `align` must be a power of two (checked in debug builds). `align_up` must not overflow
//! (also checked in debug builds); where the value is untrusted or near the top of its range,
//! use `checked_align_up`.

use std::ops::Range;

macro_rules! align_fns {
    ($t:ty, $up:ident, $checked_up:ident, $down:ident, $is_aligned:ident, $padding:ident, $range:ident) => {
        /// The smallest multiple of `align` that is at least `x`.
        #[inline(always)]
        pub const fn $up(x: $t, align: $t) -> $t {
            debug_assert!(align.is_power_of_two());
            let mask = align - 1;
            (x + mask) & !mask
        }

        /// The smallest multiple of `align` that is at least `x`, or `None` when it doesn't fit
        /// in the type.
        #[inline(always)]
        pub const fn $checked_up(x: $t, align: $t) -> Option<$t> {
            debug_assert!(align.is_power_of_two());
            let mask = align - 1;
            match x.checked_add(mask) {
                Some(it) => Some(it & !mask),
                None => None,
            }
        }

        /// The largest multiple of `align` that is at most `x`.
        #[inline(always)]
        pub const fn $down(x: $t, align: $t) -> $t {
            debug_assert!(align.is_power_of_two());
            x & !(align - 1)
        }

        /// Whether `x` is a multiple of `align`.
        #[inline(always)]
        pub const fn $is_aligned(x: $t, align: $t) -> bool {
            debug_assert!(align.is_power_of_two());
            x & (align - 1) == 0
        }

        /// The bytes from `x` to the next multiple of `align` (0 when `x` is one). Never
        /// overflows, even where aligning `x` up would.
        #[inline(always)]
        pub const fn $padding(x: $t, align: $t) -> $t {
            debug_assert!(align.is_power_of_two());
            x.wrapping_neg() & (align - 1)
        }

        /// The smallest range of whole `align`-sized blocks that covers `range`: its start
        /// aligned down and its end up (the pages a write to `range` touches).
        #[inline(always)]
        pub const fn $range(range: Range<$t>, align: $t) -> Range<$t> {
            $down(range.start, align)..$up(range.end, align)
        }
    };
}

align_fns!(usize, align_up, checked_align_up, align_down, is_aligned, padding_for, align_range);
align_fns!(u32, align_up_u32, checked_align_up_u32, align_down_u32, is_aligned_u32, padding_for_u32, align_range_u32);
align_fns!(u64, align_up_u64, checked_align_up_u64, align_down_u64, is_aligned_u64, padding_for_u64, align_range_u64);

#[cfg(all(test, not(blaze_check)))]
mod tests;
