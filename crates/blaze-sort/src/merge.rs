//! The stable comparison sort for any type, with its scratch from the caller's allocator: runs
//! of [`RUN`] sorted by insertion, then merged bottom-up, widths doubling, no recursion. A
//! merge copies the shorter run out (at most half the slice, the scratch's size) and merges
//! back into the gap, forward or backward; a merge of two runs already in order is skipped by
//! one comparison.
//!
//! core's own stable sort (driftsort, `core::slice::sort::stable::sort`) takes its scratch
//! through `BufGuard::with_capacity`, a constructor with no `self`, so only an ambient
//! allocator can serve it; rule 75 names the allocator, so the merge is written here.
//!
//! Panic safety: a comparison that panics leaves every value in the slice exactly once. The
//! insertion sort swaps; a merge holds the values not yet merged back in a guard ([`Gap`])
//! whose drop copies them into the gap, which is exactly their number of slots.

#![blaze::sans_io]

use core::ptr;

use blaze_own::{Alloc, AllocFailure, RawBuf};

/// Runs this long are sorted by insertion before the merges.
pub(crate) const RUN: usize = 20;

/// Stable insertion sort by `is_less`, by swaps: a panic in `is_less` leaves a permutation.
pub(crate) fn insertion<T>(v: &mut [T], is_less: &mut impl FnMut(&T, &T) -> bool) {
    for i in 1..v.len() {
        let mut j = i;
        while j > 0 && is_less(&v[j], &v[j - 1]) {
            v.swap(j, j - 1);
            j -= 1;
        }
    }
}

/// Stable sort of `v` by `is_less`; scratch of `v.len() / 2` values from `alloc` when `v` is
/// longer than [`RUN`].
pub(crate) fn sort<T>(
    alloc: Alloc<'_>,
    v: &mut [T],
    is_less: &mut impl FnMut(&T, &T) -> bool,
) -> Result<(), AllocFailure> {
    let n = v.len();
    if n <= RUN {
        insertion(v, is_less);
        return Ok(());
    }
    if size_of::<T>() == 0 {
        // A zero-sized type has one value: every order of it is the same bytes.
        return Ok(());
    }
    let scratch = RawBuf::<T, Alloc<'_>>::try_with_capacity_in(n / 2, alloc)?;
    for run in v.chunks_mut(RUN) {
        insertion(run, is_less);
    }
    let mut width = RUN;
    while width < n {
        let mut start = 0;
        while start + width < n {
            let mid = start + width;
            let end = (mid + width).min(n);
            if is_less(&v[mid], &v[mid - 1]) {
                // SAFETY: `scratch` holds `n / 2` values, at least the shorter run's length
                // (`min(width, end - mid) <= (end - start) / 2 <= n / 2`), and is apart from
                // `v`.
                unsafe { merge(&mut v[start..end], width, scratch.ptr(), is_less) };
            }
            start = end;
        }
        width = width.saturating_mul(2);
    }
    Ok(())
}

/// The values of `[start, end)` of the scratch, owed to the slots from `dest` on: the gap a
/// merge leaves is exactly as long. Dropped (at the end of a merge or in a panic), it copies
/// them there.
struct Gap<T> {
    start: *mut T,
    end: *mut T,
    dest: *mut T,
}

impl<T> Drop for Gap<T> {
    fn drop(&mut self) {
        // SAFETY: `[start, end)` holds initialized values moved out of the slice, `[dest,
        // dest + (end - start))` is the gap they left, and the two are in different
        // allocations (the merge's invariant).
        unsafe {
            let len = self.end.offset_from_unsigned(self.start);
            ptr::copy_nonoverlapping(self.start, self.dest, len);
        }
    }
}

/// Merges the sorted runs `v[..mid]` and `v[mid..]`, stably.
///
/// # Safety
///
/// `scratch` has room for `min(mid, v.len() - mid)` values and does not overlap `v`.
unsafe fn merge<T>(
    v: &mut [T],
    mid: usize,
    scratch: *mut T,
    is_less: &mut impl FnMut(&T, &T) -> bool,
) {
    let len = v.len();
    let base = v.as_mut_ptr();
    if mid <= len - mid {
        // Forward: the left run out to the scratch, merged with the right run into `v` from
        // the front. The gap `[dest, right)` is always the scratch's count of values long.
        // SAFETY: the contract: the scratch holds `mid` values, apart from `v`.
        unsafe { ptr::copy_nonoverlapping(base, scratch, mid) };
        // SAFETY: in bounds of the scratch's `mid` values and of `v`.
        let mut gap = Gap { start: scratch, end: unsafe { scratch.add(mid) }, dest: base };
        // SAFETY: as above.
        let (mut right, right_end) = unsafe { (base.add(mid), base.add(len)) };
        while gap.start < gap.end && right < right_end {
            // SAFETY: both point at initialized values: the right run's unmerged ones and the
            // scratch's.
            let take_right = unsafe { is_less(&*right, &*gap.start) };
            let from = if take_right { right } else { gap.start };
            // SAFETY: `gap.dest` is in the gap, which is non-empty while values are owed;
            // `from` is one of them or the right run's next, not in the gap.
            unsafe { ptr::copy_nonoverlapping(from, gap.dest, 1) };
            // SAFETY: each advance stays within its range's end.
            unsafe {
                gap.dest = gap.dest.add(1);
                if take_right {
                    right = right.add(1);
                } else {
                    gap.start = gap.start.add(1);
                }
            }
        }
        // `gap` drops: the rest of the left run fills `[dest, right_end)`.
    } else {
        // Backward: the right run out to the scratch, merged with the left run into `v` from
        // the back. The gap `[left, back)` is always the scratch's count of values long, so
        // the guard copies them to `left`.
        let right_len = len - mid;
        // SAFETY: the contract: the scratch holds `right_len` values, apart from `v`.
        unsafe { ptr::copy_nonoverlapping(base.add(mid), scratch, right_len) };
        // SAFETY: as above.
        let mut gap = Gap { start: scratch, end: unsafe { scratch.add(right_len) }, dest: unsafe { base.add(mid) } };
        // SAFETY: `base + len` is one past `v`'s end.
        let mut back = unsafe { base.add(len) };
        while base < gap.dest && gap.start < gap.end {
            // SAFETY: the last unmerged value of each run, both initialized.
            let take_left = unsafe { is_less(&*gap.end.sub(1), &*gap.dest.sub(1)) };
            // SAFETY: `back - 1` is in the gap; the value moved is the left run's last or the
            // scratch's last, neither in the gap. Then the gap is `[dest, back)` again.
            unsafe {
                back = back.sub(1);
                if take_left {
                    gap.dest = gap.dest.sub(1);
                    ptr::copy_nonoverlapping(gap.dest, back, 1);
                } else {
                    gap.end = gap.end.sub(1);
                    ptr::copy_nonoverlapping(gap.end, back, 1);
                }
            }
        }
        // `gap` drops: what is left of the right run fills `[dest, back)`.
    }
}
