//! The global allocator of a blaze binary: the system's, counted into blaze-mem's tags, so no
//! byte of the process's heap is outside blaze's one memory measure (rule 50).
//!
//! Why it exists: tempo's `blaze check` peaked at 30.19 GB of footprint while every region
//! blaze-mem counts held at most 2.67 GB (port27/tempo-deps/mem-g080807, 2026-11-17): the rest
//! was the system malloc heap, which nothing counted, so the profile could not name it. With this
//! installed (`#[global_allocator] static HEAP: blaze_mem::counted::Counted = Counted;` in the
//! binary's root), every allocation through `std::alloc::Global` (explicitly named, as rule 11
//! asks, or a std collection's default) is counted to [`crate::tag::Snapshot::heap_outside`],
//! with its churn ([`crate::tag::heap_churned`]) and high water
//! ([`crate::tag::heap_outside_high_water`]); on macOS the malloc zones' own committed bytes are
//! [`crate::tag::Snapshot::heap_committed`] (`malloc_zone_statistics`), so the gap between what
//! live blocks hold and what the zones keep is seen too.
//!
//! The cost: one thread-local add per call, and two relaxed atomic adds once per [`BATCH`] bytes
//! a thread's count moves. Each count is exact to [`BATCH`] per thread; what a thread had not
//! folded when it exited is not counted (at most [`BATCH`] each).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// How far a thread's count moves before it is folded into the shared counters.
pub const BATCH: i64 = 256 << 10;

/// The system allocator, every call counted ([module docs](self)).
pub struct Counted;

/// What one thread allocated less what it freed, and the churn, not yet folded.
struct Local {
    moved: Cell<i64>,
    allocated: Cell<u64>,
    freed: Cell<u64>,
}

std::thread_local! {
    // `const` and no destructor: reached from inside the allocator, it must neither allocate
    // nor register anything at a thread's first use.
    static LOCAL: Local = const { Local { moved: Cell::new(0), allocated: Cell::new(0), freed: Cell::new(0) } };
}

/// Counts `bytes` allocated (positive) or freed (negative) on this thread.
#[inline(always)]
fn count(bytes: i64) {
    LOCAL.with(|local| {
        let moved = local.moved.get().wrapping_add(bytes);
        local.moved.set(moved);
        match bytes >= 0 {
            true => local.allocated.set(local.allocated.get().wrapping_add(bytes.unsigned_abs())),
            false => local.freed.set(local.freed.get().wrapping_add(bytes.unsigned_abs())),
        }
        if moved >= BATCH || moved <= -BATCH {
            fold(local);
        }
    });
}

#[cold]
#[inline(never)]
fn fold(local: &Local) {
    committed_said();
    crate::tag::heap_moved(None, local.moved.replace(0));
    crate::tag::heap_churned(None, local.allocated.replace(0), local.freed.replace(0));
}

/// Tells blaze-mem how to read what the malloc zones committed, at the first fold: takes no
/// lock and allocates nothing (`tag::heap_committed_by`).
#[inline]
fn committed_said() {
    #[cfg(target_os = "macos")]
    crate::tag::heap_committed_by(zones::committed);
}

/// macOS's malloc zones, asked what they hold (public API, `<malloc/malloc.h>`).
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod zones {
    /// `malloc_statistics_t`.
    #[repr(C)]
    #[derive(Default)]
    struct Statistics {
        blocks_in_use: u32,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }

    unsafe extern "C" {
        /// `void malloc_zone_statistics(malloc_zone_t *zone, malloc_statistics_t *stats)`: with
        /// a null zone, every zone's summed.
        fn malloc_zone_statistics(zone: *mut core::ffi::c_void, stats: *mut Statistics);
    }

    /// The bytes every malloc zone has allocated from the system: its live blocks and the free
    /// ones it keeps. Takes the zones' locks; called by a snapshot, never by the allocator.
    pub(super) fn committed() -> u64 {
        let mut stats = Statistics::default();
        // SAFETY: a null zone asks for every zone's sum (malloc.h); `stats` is a live, writable
        // `malloc_statistics_t` of the C layout (`repr(C)`, the header's four fields in order).
        unsafe { malloc_zone_statistics(core::ptr::null_mut(), &mut stats) };
        stats.size_allocated as u64
    }
}

/// A layout's size as a signed count: at most `isize::MAX`.
fn signed(bytes: usize) -> i64 {
    bytes as i64
}

// SAFETY: every call goes to `System` with the caller's own arguments, so `System`'s guarantees
// hold; the counting beside it touches only this thread's `const` thread-local, which has no
// destructor and so registers nothing, and relaxed atomics (`tag::heap_moved`,
// `tag::heap_churned`, `tag::heap_committed_by`'s `OnceLock::set` of a `fn` pointer, which
// allocates nothing). Nothing here allocates, so no call reenters the allocator.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract for `layout`.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            count(signed(layout.size()));
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc_zeroed`'s contract for `layout`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            count(signed(layout.size()));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller says `ptr` was allocated by this allocator, which is `System`'s,
        // with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        count(-signed(layout.size()));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `realloc`'s contract for `ptr`, `layout` and `new_size`.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            count(signed(new_size) - signed(layout.size()));
        }
        new
    }
}
