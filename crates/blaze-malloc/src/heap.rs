//! A thread's heap: per class, a list of spans with free blocks (its head the one allocating),
//! and per span kind, a list of free spans. Only its owner thread touches these, so allocating
//! and freeing the owner's blocks takes no lock and no atomic operation; other threads reach a
//! heap only through its queue ([`Remote`]) and its counters.
//!
//! The fast path of an allocation: the class from the size (a table load), the heap from the
//! thread (`tls`), the head span of the class, its free list's first block, unlinked; or, with
//! the list empty, the next block cut from the span's uncut rest. An empty class's head is a
//! static empty span (`EMPTY`), so the fast path has no null check of its own: the empty span's
//! list and rest are empty, and it takes the slow path, which never writes it.
//!
//! The fast path of a free: the header from the address (`segment::header_of`), the span from
//! the offset, then a compare of the span's heap with the thread's: equal, the block goes on the
//! span's free list; else on its heap's queue.

use core::{cell::UnsafeCell, ptr};

use crate::{
    MallocError,
    adopt::{Claimed, Registry},
    class::{self, CLASSES},
    remote::{Block, Freed, Remote},
    segment::{self, Kind, Segment, Span, State},
    sync::atomic::{AtomicU64, Ordering},
    tls,
};

/// The most heaps alive at once: one per thread that allocated and has not ended. A thread past
/// it gets a typed refusal ([`MallocError::Heaps`]), never a shared heap.
pub(crate) const MAX_HEAPS: usize = 1024;

/// A heap's owner-only state.
struct Own {
    /// Per class, the head of its list of spans (`EMPTY` when none).
    bins: [*mut Span; CLASSES],
    /// Per span kind (small, medium), the head of its free spans (null when none).
    free: [*mut Span; 2],
}

/// A value on a 128-byte line of its own: the queue's head is written by other threads, and must
/// not share a line with the owner's bins.
#[repr(C, align(128))]
struct Line<T>(T);

/// A heap's counters, read by any thread ([`crate::stats`]). The owner moves its own with a
/// relaxed load and store (a plain one on arm64 and x86), never a read-modify-write.
pub(crate) struct Counters {
    /// Bytes of small blocks out, by class size.
    pub(crate) bytes: AtomicU64,
    /// Bytes of large blocks out (moved by any thread that frees one, so by `fetch_add`).
    pub(crate) large: AtomicU64,
    /// Spans in use.
    pub(crate) spans: AtomicU64,
    /// Segments held.
    pub(crate) segments: AtomicU64,
    /// Blocks other threads freed into this heap, counted as the owner takes them, and their
    /// bytes.
    pub(crate) remote: AtomicU64,
    pub(crate) remote_bytes: AtomicU64,
}

#[repr(C, align(128))]
pub(crate) struct Heap {
    own: UnsafeCell<Own>,
    remote: Line<Remote>,
    pub(crate) counters: Counters,
}

// SAFETY: `own` is touched only by the heap's owner thread (the one whose `tls` names it, or the
// one adopting it, after the registry's acquire); `remote` and `counters` are atomics.
unsafe impl Sync for Heap {}

/// The empty span every empty class's head points at: its list and rest are empty, so the fast
/// path goes slow, which never writes it.
struct Empty(UnsafeCell<Span>);

// SAFETY: never written (the slow path checks for it before any write).
unsafe impl Sync for Empty {}

static EMPTY: Empty = Empty(UnsafeCell::new(Span::EMPTY));

fn empty() -> *mut Span {
    EMPTY.0.get()
}

impl Heap {
    const fn new() -> Heap {
        Heap {
            own: UnsafeCell::new(Own {
                bins: [EMPTY.0.get(); CLASSES],
                free: [ptr::null_mut(); 2],
            }),
            remote: Line(Remote::new()),
            counters: Counters {
                bytes: AtomicU64::new(0),
                large: AtomicU64::new(0),
                spans: AtomicU64::new(0),
                segments: AtomicU64::new(0),
                remote: AtomicU64::new(0),
                remote_bytes: AtomicU64::new(0),
            },
        }
    }

    #[inline(always)]
    fn own(&self) -> *mut Own {
        self.own.get()
    }

    /// The heap's index in [`HEAPS`].
    pub(crate) fn index(&self) -> u32 {
        let first = HEAPS.as_ptr().addr();
        ((ptr::from_ref(self).addr() - first) / size_of::<Heap>()) as u32
    }

    /// Moves an owner-only counter by `delta` bytes, up or down.
    #[inline(always)]
    fn bump(counter: &AtomicU64, add: u64, sub: u64) {
        counter.store(counter.load(Ordering::Relaxed).wrapping_add(add).wrapping_sub(sub), Ordering::Relaxed);
    }

    /// Counts a large block of `size` bytes in or out; any thread.
    pub(crate) fn count_large(&self, size: usize, alloc: bool) {
        if alloc {
            self.counters.large.fetch_add(size as u64, Ordering::Relaxed);
        } else {
            self.counters.large.fetch_sub(size as u64, Ordering::Relaxed);
        }
    }
}

/// Every heap there can be, in the process's zeroed data: an untouched one costs no memory.
pub(crate) static HEAPS: [Heap; MAX_HEAPS] = [const { Heap::new() }; MAX_HEAPS];

/// Which slots are taken, and the heaps of ended threads to adopt.
pub(crate) static REGISTRY: Registry<MAX_HEAPS> = Registry::new();

/// The heap of the thread, made or adopted on its first allocation.
#[inline(always)]
pub(crate) fn current() -> Result<&'static Heap, MallocError> {
    let heap = tls::current();
    if heap.is_null() {
        return start();
    }
    // SAFETY: `tls` holds only pointers into `HEAPS`, a static.
    Ok(unsafe { &*heap })
}

/// Gives the thread a heap: an ended thread's, adopted (its queue's blocks freed into their
/// spans), or a fresh one. Allocates nothing: the slot is a static, and the thread-local word
/// is set before the destructor is registered, so a registration that allocates (glibc's for a
/// high key) finds the heap there.
#[cold]
#[inline(never)]
fn start() -> Result<&'static Heap, MallocError> {
    tls::prepare()?;
    let claimed = REGISTRY.claim(
        |at| &HEAPS[at as usize].remote.0,
        // SAFETY: an adopted heap's queue holds blocks of its spans, each given up by its freer;
        // this thread owns the heap from the claim's acquire on.
        |block| unsafe { free_owned(block.cast()) },
    );
    let Some(claimed) = claimed else { return Err(MallocError::Heaps { max: MAX_HEAPS }) };
    let heap = &HEAPS[claimed.index() as usize];
    if let Claimed::Adopted(_) = claimed {
        tls::count_adoption();
    }
    tls::install(heap)?;
    Ok(heap)
}

/// What the thread-end destructor does with the heap: takes in what other threads freed, then
/// leaves it for another thread to adopt.
pub(crate) fn abandon(heap: &'static Heap) {
    // SAFETY: the ending thread owns the heap until the abandon below publishes it.
    let _ = unsafe { heap.remote.0.drain(|block| free_owned(block.cast())) };
    REGISTRY.abandon(heap.index());
}

// ---------------------------------------------------------------------------------------------
// Allocation.

/// A block of class `class` from `heap`, zeroed when `zero`.
///
/// # Safety
///
/// The calling thread owns `heap`.
#[inline(always)]
pub(crate) unsafe fn alloc(heap: &Heap, class: usize, zero: bool) -> Result<*mut u8, MallocError> {
    let own = heap.own();
    loop {
        // SAFETY: the caller owns the heap, so its bins are this thread's; each head is a live
        // span (or `EMPTY`, whose list and rest are empty and which is only read).
        unsafe {
            let span = (*own).bins[class];
            let block = (*span).free;
            if !block.is_null() {
                (*span).free = (*block).next;
                (*span).used += 1;
                let size = (*span).size as usize;
                Heap::bump(&heap.counters.bytes, size as u64, 0);
                if zero {
                    ptr::write_bytes(block.cast::<u8>(), 0, size);
                }
                return Ok(block.cast());
            }
            let bump = (*span).bump;
            if bump < (*span).end {
                let size = (*span).size as usize;
                (*span).bump = bump.add(size);
                (*span).used += 1;
                Heap::bump(&heap.counters.bytes, size as u64, 0);
                if zero && !(*span).fresh {
                    ptr::write_bytes(bump, 0, size);
                }
                return Ok(bump);
            }
            refill(heap, class)?;
        }
    }
}

/// Makes the head of class `class` a span with a block to give: takes an exhausted head off the
/// list (full), moves to the next span, takes in what other threads freed, or cuts a new span.
///
/// # Safety
///
/// The calling thread owns `heap`.
#[cold]
#[inline(never)]
unsafe fn refill(heap: &Heap, class: usize) -> Result<(), MallocError> {
    let own = heap.own();
    // SAFETY: the caller owns the heap; the bins hold live spans of its segments.
    unsafe {
        let head = (*own).bins[class];
        if head != empty() {
            unlink_bin(own, head);
            (*head).state = State::Full;
            if (*own).bins[class] != empty() {
                return Ok(());
            }
        }
        if !heap.remote.0.is_empty() {
            let _ = heap.remote.0.drain(|block| free_owned(block.cast()));
            if (*own).bins[class] != empty() {
                return Ok(());
            }
        }
        let kind = if class::is_small(class) { Kind::Small } else { Kind::Medium };
        let span = take_span(heap, kind)?;
        let seg = segment_of(span);
        let shift = u32::from((*seg).shift);
        let slot = usize::from((*span).slot);
        let start = segment::span_start(seg, shift, slot);
        (*seg).map.ensure((slot + 1) << shift)?;
        let size = class::SIZES[class];
        let blocks = (1usize << shift) / size as usize;
        (*span).free = ptr::null_mut();
        (*span).bump = start;
        (*span).end = start.add(blocks * size as usize);
        (*span).heap = heap;
        (*span).used = 0;
        (*span).size = size;
        (*span).class = class as u8;
        (*span).state = State::Bin;
        (*span).prev = ptr::null_mut();
        (*span).next = ptr::null_mut();
        push_bin(own, span);
        Heap::bump(&heap.counters.spans, 1, 0);
    }
    Ok(())
}

/// A free span of `kind` from `heap`'s list, or from a new segment.
///
/// # Safety
///
/// The calling thread owns `heap`.
unsafe fn take_span(heap: &Heap, kind: Kind) -> Result<*mut Span, MallocError> {
    let own = heap.own();
    let k = kind as usize;
    // SAFETY: the caller owns the heap; its free lists hold free spans of its segments.
    unsafe {
        if (*own).free[k].is_null() {
            let seg = segment::acquire(kind, heap)?;
            Heap::bump(&heap.counters.segments, 1, 0);
            // Highest slot first, so the lowest is on top and spans are cut in address order,
            // which the region's prefix commit follows.
            for slot in (1..usize::from((*seg).slots)).rev() {
                push_free(own, k, &raw mut (*seg).spans[slot]);
            }
        }
        let span = (*own).free[k];
        unlink_free(own, k, span);
        (*segment_of(span)).used += 1;
        Ok(span)
    }
}

// ---------------------------------------------------------------------------------------------
// Freeing.

/// The segment whose header holds `span`.
#[inline(always)]
fn segment_of(span: *mut Span) -> *mut Segment {
    let off = span.addr() & (segment::SEG - 1);
    span.cast::<u8>().wrapping_sub(off).cast()
}

/// Frees `block`, a small block another thread freed into its heap's queue, on the heap's owner
/// thread (the thread the heap is, or one adopting or ending it), counting it as a remote free.
///
/// # Safety
///
/// `block` is a live small block whose span's heap the calling thread owns, given up.
#[inline]
unsafe fn free_owned(block: *mut u8) {
    let at = segment::Given::of(block);
    // SAFETY: a live small block (the caller's).
    let (header, block) = unsafe { segment::header_of(at).small() };
    // SAFETY: a live small block's header is its live segment, its span's heap set while the
    // block is out.
    unsafe {
        let span = &raw mut (*header).spans[(at.addr() & (segment::SEG - 1)) >> (*header).shift];
        let heap = &*(*span).heap;
        let size = u64::from((*span).size);
        Heap::bump(&heap.counters.remote, 1, 0);
        Heap::bump(&heap.counters.remote_bytes, size, 0);
        // No caller's pointer is at hand here: the block was given up by a `dealloc` on another
        // thread, which wrote its queue link through the caller's pointer, and the owner now
        // re-links it through the allocator's (a later write, after that free returned).
        free_local(heap, span, block, Freed::of(block));
    }
}

/// Frees a small block into its span, on the span's owner thread: the span's list keeps `block`
/// (the allocator's pointer), and the link is written through `freed` (the pointer the free was
/// given, [`Freed`]).
///
/// # Safety
///
/// `span` is `block`'s span, `heap` its heap, which the calling thread owns; `block` has the
/// allocator's provenance ([`segment::header_of`]), not a caller's; `freed` points at the same
/// block.
#[inline(always)]
pub(crate) unsafe fn free_local(heap: &Heap, span: *mut Span, block: *mut u8, freed: Freed) {
    let block = block.cast::<Block>();
    // SAFETY: the caller owns the span; the block is given up, so its first word is ours to
    // write, through the pointer it was given back by.
    unsafe {
        freed.link((*span).free);
        (*span).free = block;
        (*span).used -= 1;
        Heap::bump(&heap.counters.bytes, 0, u64::from((*span).size));
        if (*span).used == 0 || (*span).state != State::Bin {
            settle(heap, span);
        }
    }
}

/// After a free: puts a full span back on its class's list, and gives back a span with no block
/// out unless it is its class's only one.
///
/// # Safety
///
/// As for [`free_local`].
#[cold]
#[inline(never)]
unsafe fn settle(heap: &Heap, span: *mut Span) {
    let own = heap.own();
    // SAFETY: the caller owns the span and its heap.
    unsafe {
        let class = usize::from((*span).class);
        if (*span).state == State::Full {
            (*span).state = State::Bin;
            // Second, behind the head that is allocating: a span with one free block as the
            // head would run out at once and go full again.
            let head = (*own).bins[class];
            if head == empty() {
                push_bin(own, span);
            } else {
                (*span).prev = head;
                (*span).next = (*head).next;
                if !(*head).next.is_null() {
                    (*(*head).next).prev = span;
                }
                (*head).next = span;
            }
        }
        if (*span).used != 0 {
            return;
        }
        if (*own).bins[class] == span && (*span).next.is_null() {
            return;
        }
        unlink_bin(own, span);
        let seg = segment_of(span);
        let k = (*seg).kind as usize;
        (*span).state = State::Free;
        (*span).heap = ptr::null();
        (*span).free = ptr::null_mut();
        (*span).fresh = false;
        push_free(own, k, span);
        Heap::bump(&heap.counters.spans, 0, 1);
        (*seg).used -= 1;
        if (*seg).used == 0 {
            for slot in 1..usize::from((*seg).slots) {
                unlink_free(own, k, &raw mut (*seg).spans[slot]);
            }
            Heap::bump(&heap.counters.segments, 0, 1);
            segment::retire(seg);
        }
    }
}

/// Frees the block at `at` from any thread; `freed` is the same pointer the caller handed back,
/// used only to write the block's link ([`Freed`]).
///
/// # Safety
///
/// The block at `at` is a live block of this allocator, given up, and `freed` the caller's
/// pointer to it.
#[inline(always)]
pub(crate) unsafe fn free(at: segment::Given, freed: Freed) {
    let found = segment::header_of(at);
    // SAFETY: the caller's: a live block.
    if unsafe { found.kind() } == Kind::Large {
        // SAFETY: as above, a large one.
        return unsafe { segment::large_free(found.large()) };
    }
    // SAFETY: as above, a small one.
    let (seg, block) = unsafe { found.small() };
    // SAFETY: a live small block's header is its live segment, and its span's `heap` is set
    // while the block is out; `block` has the mapping's provenance over the block.
    unsafe {
        let span = &raw mut (*seg).spans[(at.addr() & (segment::SEG - 1)) >> (*seg).shift];
        let owner = (*span).heap;
        if owner == tls::current() {
            free_local(&*owner, span, block, freed);
        } else {
            (*owner).remote.0.push(block.cast(), freed);
        }
    }
}

/// The usable size of the live block at `at`.
///
/// # Safety
///
/// The block at `at` is a live block of this allocator.
#[inline]
pub(crate) unsafe fn usable(at: segment::Given) -> usize {
    let found = segment::header_of(at);
    // SAFETY: as for `free`.
    unsafe {
        if found.kind() == Kind::Large {
            return segment::large_size(found.large());
        }
        let (seg, _) = found.small();
        (*seg).spans[(at.addr() & (segment::SEG - 1)) >> (*seg).shift].size as usize
    }
}

// ---------------------------------------------------------------------------------------------
// Lists.

/// Puts `span` at the head of its class's list.
unsafe fn push_bin(own: *mut Own, span: *mut Span) {
    // SAFETY: the caller owns the heap and the span.
    unsafe {
        let class = usize::from((*span).class);
        let head = (*own).bins[class];
        (*span).prev = ptr::null_mut();
        (*span).next = if head == empty() { ptr::null_mut() } else { head };
        if head != empty() {
            (*head).prev = span;
        }
        (*own).bins[class] = span;
    }
}

/// Takes `span` off its class's list.
unsafe fn unlink_bin(own: *mut Own, span: *mut Span) {
    // SAFETY: the caller owns the heap and the span, which is on its class's list.
    unsafe {
        let class = usize::from((*span).class);
        let (prev, next) = ((*span).prev, (*span).next);
        if prev.is_null() {
            (*own).bins[class] = if next.is_null() { empty() } else { next };
        } else {
            (*prev).next = next;
        }
        if !next.is_null() {
            (*next).prev = prev;
        }
        (*span).prev = ptr::null_mut();
        (*span).next = ptr::null_mut();
    }
}

unsafe fn push_free(own: *mut Own, k: usize, span: *mut Span) {
    // SAFETY: the caller owns the heap and the free span.
    unsafe {
        let head = (*own).free[k];
        (*span).prev = ptr::null_mut();
        (*span).next = head;
        if !head.is_null() {
            (*head).prev = span;
        }
        (*own).free[k] = span;
    }
}

unsafe fn unlink_free(own: *mut Own, k: usize, span: *mut Span) {
    // SAFETY: the caller owns the heap and the span, which is on its kind's free list.
    unsafe {
        let (prev, next) = ((*span).prev, (*span).next);
        if prev.is_null() {
            (*own).free[k] = next;
        } else {
            (*prev).next = next;
        }
        if !next.is_null() {
            (*next).prev = prev;
        }
        (*span).prev = ptr::null_mut();
        (*span).next = ptr::null_mut();
    }
}
