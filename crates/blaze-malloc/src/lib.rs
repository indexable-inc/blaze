//! blaze's general-purpose allocator (docs/v2/plan/design/malloc.md): for what is not in a
//! frame, an arena or a pool of blaze-mem, and for bstd programs that choose one. The shape the
//! best current allocators share (mimalloc, snmalloc, TCMalloc):
//!
//! - A heap per thread ([`heap`]): its own blocks are allocated and freed with no lock and no
//!   atomic read-modify-write.
//! - 81 size classes to 64 KiB, 12.5% apart past 128 bytes ([`class`]), each served from spans
//!   of 64 KiB (classes to 8 KiB) or 512 KiB cut into blocks of one class. A block has no header:
//!   its segment (2 MiB, aligned to its size) is its address with the low 21 bits cleared, its
//!   span the next bits, and the span's descriptor in the segment's header holds its class, its
//!   free list and its heap ([`segment`]).
//! - A free from another thread is pushed on the owning heap's queue ([`remote`]), one
//!   compare-and-swap; the owner takes the whole queue with one swap when it next runs short.
//! - Large blocks (past 64 KiB, or aligned past it) are a blaze-mem `Region` each, reserved with
//!   room to grow in place, kept in a small cache when freed; `alloc_zeroed` fills only memory
//!   that is not fresh from the system.
//! - A thread that ends leaves its heap to be adopted whole by the next thread that needs one
//!   ([`adopt`]): its blocks stay valid, and frees into them reach the adopter.
//!
//! The floor (rule 67): an allocate-then-free pair of a small block is a class lookup, the
//! thread's heap, a pop and a push on a span's free list and two counter updates: on the order of
//! twenty instructions, about 1.1 ns on an M5 at some four instructions a cycle. Measured
//! (`examples/cost.rs`, 2026-11-16): 3.8 to 4.0 ns a pair, some 3.5 times the floor; the
//! example's rows give each ratio to the system's allocator.
//!
//! Unsafe code is the allocator's whole job: every block says what it rests on. Nothing here
//! prints; the statistics are counters a caller reads ([`stats`], [`thread_stats`]).

#![no_std]
#![feature(allocator_api, thread_local)]
// blaze's lints name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "a general-purpose allocator: blocks found by address arithmetic from headers it wrote, free lists threaded through freed blocks, a lock-free queue of blocks per heap, and the thread's TSD slot read directly on macOS; each block names what it rests on"
)]
#![allow(
    unsafe_code,
    reason = "an allocator's every path turns addresses into places: each `unsafe` says why it holds"
)]

#[cfg(any(test, blaze_check))]
extern crate std;

mod adopt;
mod class;
mod heap;
mod remote;
mod segment;
mod sync;
mod tls;

#[cfg(all(test, not(blaze_check)))]
mod tests;

#[cfg(all(test, blaze_check))]
mod model;

use core::{
    alloc::{AllocError, Allocator, GlobalAlloc, Layout},
    fmt,
    ptr::{self, NonNull},
};

pub use blaze_mem::huge::Pages;
pub use segment::{set_large_pages, trim};

use crate::sync::atomic::Ordering;

/// Why a block could not be had. Each names what was asked.
#[derive(Debug)]
pub enum MallocError {
    /// The page source (blaze-mem's regions) refused: the system's limit.
    Os(blaze_mem::MemError),
    /// Every heap slot is held by a thread alive now.
    Heaps { max: usize },
    /// A size and alignment no mapping can hold.
    TooLarge { size: usize, align: usize },
    /// A thread call the allocator needs failed, with its code.
    Thread { call: &'static str, code: i32 },
}

impl fmt::Display for MallocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MallocError::Os(why) => write!(f, "blaze-malloc: the page source refused: {why}"),
            MallocError::Heaps { max } => {
                write!(f, "blaze-malloc: all {max} heaps are held by threads alive now")
            }
            MallocError::TooLarge { size, align } => {
                write!(f, "blaze-malloc: {size} bytes aligned to {align} pass the address space")
            }
            MallocError::Thread { call, code } => {
                write!(f, "blaze-malloc: `{call}` failed with {code}")
            }
        }
    }
}

/// The allocator: a unit value, every instance the same process-wide heaps. `#[global_allocator]`
/// or an `Allocator` parameter.
#[derive(Clone, Copy, Debug, Default)]
pub struct Malloc;

impl Malloc {
    /// A block for `layout`, its usable size the slice's length; a typed error when none.
    #[inline(never)]
    pub fn try_allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, MallocError> {
        // SAFETY: a fresh block for the layout, never null on `Ok`.
        let block = unsafe { allocate(layout, false) }?;
        Ok(slice(block, layout.size()))
    }

    /// The bytes the live block at `block` can hold: its class's size, or a large block's.
    ///
    /// # Safety
    ///
    /// `block` is a live block of this allocator.
    pub unsafe fn usable_size(&self, block: NonNull<u8>) -> usize {
        // SAFETY: the caller's.
        unsafe { heap::usable(segment::Given::of(block.as_ptr())) }
    }
}

fn slice(block: *mut u8, len: usize) -> NonNull<[u8]> {
    // SAFETY: `allocate` never returns null on `Ok`.
    let block = unsafe { NonNull::new_unchecked(block) };
    NonNull::slice_from_raw_parts(block, len)
}

/// A block for `layout`, zeroed when `zero`.
///
/// # Safety
///
/// None beyond the layout's own; unsafe because it hands out raw memory.
#[inline(always)]
unsafe fn allocate(layout: Layout, zero: bool) -> Result<*mut u8, MallocError> {
    let (size, align) = (layout.size(), layout.align());
    let class = if align <= 8 && size <= class::MAX {
        class::of(size)
    } else {
        match class::of_aligned(size, align) {
            Some(class) => class,
            None => return segment::large_alloc(layout, heap::current()?, zero),
        }
    };
    let heap = heap::current()?;
    // SAFETY: `current` is the calling thread's own heap.
    unsafe { heap::alloc(heap, class, zero) }
}

/// Moves the block at `block` (of `old`) to `new`: in place when it meets `new`'s alignment and
/// its class still fits it (at most its size and more than half of it) or its large mapping
/// holds it, else a new block, copied, and the old freed.
///
/// # Safety
///
/// `block` is a live block of this allocator, given up on success.
unsafe fn reallocate(block: *mut u8, old: Layout, new: Layout) -> Result<*mut u8, MallocError> {
    let new_size = new.size();
    let at = segment::Given::of(block);
    if block.addr() % new.align() == 0 {
        // The caller's pointer reaches its own block only: the header through `header_of`.
        let found = segment::header_of(at);
        // SAFETY: a live block's header is live.
        if unsafe { found.kind() } == segment::Kind::Large {
            // SAFETY: as above, a large header.
            if new_size > class::MAX && unsafe { segment::large_resize(found.large(), new_size) }?
            {
                return Ok(block);
            }
        } else {
            // SAFETY: as above.
            let size = unsafe { heap::usable(at) };
            if new_size <= size && new_size > size / 2 {
                return Ok(block);
            }
        }
    }
    // SAFETY: a fresh block.
    let moved = unsafe { allocate(new, false) }?;
    // SAFETY: both blocks are live and distinct; `old.size()` and `new_size` bound what each
    // holds.
    unsafe { ptr::copy_nonoverlapping(block, moved, old.size().min(new_size)) };
    // SAFETY: the caller gave the old block up.
    unsafe { heap::free(at, remote::Freed::of(block)) };
    Ok(moved)
}

// SAFETY: every block is distinct while live (a span's free list and uncut rest are disjoint,
// spans are disjoint slots of a segment, mappings are distinct reservations), aligned to the
// layout (`class::of_aligned`, `large_alloc`), and of at least its size.
//
// Every entry point is `#[inline(never)]`: callers must see an allocate and a free as the opaque
// calls they are, never the allocator's body. Inlined, a caller's optimizer would see the same
// bytes handed out, given back and handed out again under pointers it may assume are unrelated
// (the allocate call's result is `noalias`), and could move a store of the old owner past a
// store of the new one. Every allocator behind these symbols relies on the call staying a call
// (docs/v2/plan/design/malloc.md, "The contract with compiled code").
unsafe impl GlobalAlloc for Malloc {
    #[inline(never)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the layout is the caller's.
        unsafe { allocate(layout, false) }.unwrap_or(ptr::null_mut())
    }

    #[inline(never)]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as above.
        unsafe { allocate(layout, true) }.unwrap_or(ptr::null_mut())
    }

    #[inline(never)]
    unsafe fn dealloc(&self, block: *mut u8, _layout: Layout) {
        // SAFETY: the caller gives up a live block of ours.
        unsafe { heap::free(segment::Given::of(block), remote::Freed::of(block)) }
    }

    #[inline(never)]
    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let Ok(new) = Layout::from_size_align(new_size, layout.align()) else {
            return ptr::null_mut();
        };
        // SAFETY: the caller's.
        unsafe { reallocate(block, layout, new) }.unwrap_or(ptr::null_mut())
    }
}

// SAFETY: as for `GlobalAlloc`; the unit value has no state of its own, so copies and moves of it
// share every block.
unsafe impl Allocator for Malloc {
    #[inline(never)]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: a fresh block.
        let block = unsafe { allocate(layout, false) }.map_err(|_| AllocError)?;
        Ok(slice(block, layout.size()))
    }

    #[inline(never)]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: as above.
        let block = unsafe { allocate(layout, true) }.map_err(|_| AllocError)?;
        Ok(slice(block, layout.size()))
    }

    #[inline(never)]
    unsafe fn deallocate(&self, block: NonNull<u8>, _layout: Layout) {
        // SAFETY: the caller gives up a live block of ours.
        unsafe { heap::free(segment::Given::of(block.as_ptr()), remote::Freed::of(block.as_ptr())) }
    }

    unsafe fn grow(
        &self,
        block: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's.
        unsafe { self.moved(block, old, new) }
    }

    unsafe fn shrink(
        &self,
        block: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's.
        unsafe { self.moved(block, old, new) }
    }
}

impl Malloc {
    /// `grow` and `shrink`: in place, or moved (a stricter alignment the block misses moves).
    ///
    /// # Safety
    ///
    /// As for [`Allocator::grow`].
    unsafe fn moved(
        &self,
        block: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's.
        let moved =
            unsafe { reallocate(block.as_ptr(), old, new) }.map_err(|_| AllocError)?;
        Ok(slice(moved, new.size()))
    }
}

/// What the allocator holds, from its counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Bytes of small blocks out, by class size.
    pub small_bytes: u64,
    /// Bytes of large blocks out, by requested size.
    pub large_bytes: u64,
    /// Spans in use.
    pub spans: u64,
    /// Segments heaps hold.
    pub segments: u64,
    /// Blocks freed by a thread other than their heap's, taken back by the owner.
    pub cross_thread_frees: u64,
    /// Their bytes, by class size.
    pub cross_thread_bytes: u64,
}

impl Stats {
    /// Bytes live: small and large.
    pub fn bytes_live(&self) -> u64 {
        self.small_bytes.wrapping_add(self.large_bytes)
    }

    fn of(heap: &heap::Heap) -> Stats {
        let c = &heap.counters;
        Stats {
            small_bytes: c.bytes.load(Ordering::Relaxed),
            large_bytes: c.large.load(Ordering::Relaxed),
            spans: c.spans.load(Ordering::Relaxed),
            segments: c.segments.load(Ordering::Relaxed),
            cross_thread_frees: c.remote.load(Ordering::Relaxed),
            cross_thread_bytes: c.remote_bytes.load(Ordering::Relaxed),
        }
    }

    fn add(self, other: Stats) -> Stats {
        Stats {
            small_bytes: self.small_bytes.wrapping_add(other.small_bytes),
            large_bytes: self.large_bytes.wrapping_add(other.large_bytes),
            spans: self.spans + other.spans,
            segments: self.segments + other.segments,
            cross_thread_frees: self.cross_thread_frees + other.cross_thread_frees,
            cross_thread_bytes: self.cross_thread_bytes + other.cross_thread_bytes,
        }
    }
}

/// Every heap's counters summed.
pub fn stats() -> Stats {
    heap::HEAPS.iter().map(Stats::of).fold(Stats::default(), Stats::add)
}

/// The calling thread's heap's counters (zero before its first allocation).
pub fn thread_stats() -> Stats {
    let heap = tls::current();
    if heap.is_null() {
        return Stats::default();
    }
    // SAFETY: `tls` holds only `HEAPS` addresses.
    Stats::of(unsafe { &*heap })
}

/// The page source's process-wide counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Source {
    /// Segments reserved now (held by heaps or cached).
    pub segments: u64,
    /// Of them, in the cache.
    pub segments_cached: u64,
    /// Large mappings reserved now (live or cached).
    pub large: u64,
    /// Of them, in the cache.
    pub large_cached: u64,
    /// Heaps adopted from ended threads.
    pub adopted: u64,
}

/// The page source's counts.
pub fn source() -> Source {
    let (segments, segments_cached, large, large_cached) = segment::counts();
    Source {
        segments,
        segments_cached,
        large,
        large_cached,
        adopted: tls::ADOPTED.load(Ordering::Relaxed),
    }
}

/// Ways to reach a thread's word, for the cost example's `tls` rows (rule 45): not API.
#[doc(hidden)]
pub mod probe {
    use core::{cell::Cell, ffi::c_void, ptr};

    #[thread_local]
    static WORD: Cell<*const u8> = Cell::new(ptr::null());

    /// A `#[thread_local]` word's read.
    #[inline(always)]
    pub fn thread_local() -> *const u8 {
        WORD.get()
    }

    /// `pthread_getspecific` of the allocator's key.
    #[inline(always)]
    pub fn pthread() -> *mut c_void {
        crate::tls::by_call()
    }

    /// The allocator's own read (`tls::current`).
    #[inline(always)]
    pub fn chosen() -> *const u8 {
        crate::tls::current().cast()
    }

    /// Whether the thread has a heap yet (the key made and set).
    pub fn ready() -> bool {
        crate::tls::key() != 0 && !crate::tls::current().is_null()
    }
}
