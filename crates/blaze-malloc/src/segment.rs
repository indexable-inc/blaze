//! The page source: segments of 2 MiB, each one [`Mapping`] (a blaze-mem [`Region`], aligned to
//! 2 MiB by its reservation), cut into spans of one size; and large blocks, each a mapping of its
//! own. Every header sits at the start of its 2 MiB granule, so a block finds its header by
//! clearing the low 21 bits of its address: no page map, no header in front of a block.
//!
//! A small segment is 32 slots of 64 KiB, a medium one 4 of 512 KiB; slot 0 holds the header
//! and is otherwise untouched (address space only: its pages past the header are never written,
//! so they cost no memory). A span's slot is `offset >> shift`, its descriptor the header's
//! `spans[slot]`.
//!
//! A large block's header is at its granule's start too, so its block starts past the header
//! inside the first granule; a block aligned to 2 MiB or more starts at a granule boundary, and
//! its header is then the granule before (`header_of`). Small blocks never start at a boundary
//! (slot 0 is the header's), so the boundary case is only ever a large block.
//!
//! Empty segments and freed large mappings go to two caches under a short lock: up to [`HOT`]
//! segments kept as they are, then up to [`COLD`] whose pages are given back first
//! ([`Region::purge`], and [`Region::reuse`] before they are written again), then released. The
//! decay is by count, not by clock: nothing here reads the time.

use core::{alloc::Layout, cell::UnsafeCell, ptr};

use blaze_mem::huge::Pages;

use crate::{
    MallocError,
    heap::Heap,
    remote::Block,
    sync::{
        Spin,
        atomic::{AtomicU64, AtomicU8, Ordering},
    },
};

/// A segment's size and alignment: blaze-mem's reservations are aligned to a huge page.
///
/// Under Miri, the one place the page source is sized for the interpreter: 512 KiB segments of
/// 16 KiB small spans and 128 KiB medium ones, so every class still has a span of at least two
/// blocks and the layout (32 small slots, 4 medium, the header in slot 0) is the same shape,
/// while a segment costs the interpreter a quarter of the memory. The in-crate tests and a
/// `#[global_allocator]` install both reserve through [`Mapping::reserve`] with these sizes.
#[cfg(not(miri))]
pub(crate) const SEG: usize = blaze_mem::HUGE_PAGE;
#[cfg(miri)]
pub(crate) const SEG: usize = 512 << 10;

/// The most spans a segment holds descriptors for (small: 32 slots).
pub(crate) const MAX_SPANS: usize = SEG >> SMALL_SHIFT;

/// A small span's size, as a shift: 64 KiB (16 KiB under Miri).
#[cfg(not(miri))]
pub(crate) const SMALL_SHIFT: u32 = 16;
#[cfg(miri)]
pub(crate) const SMALL_SHIFT: u32 = 14;

/// A medium span's size, as a shift: 512 KiB (128 KiB under Miri).
#[cfg(not(miri))]
pub(crate) const MEDIUM_SHIFT: u32 = 19;
#[cfg(miri)]
pub(crate) const MEDIUM_SHIFT: u32 = 17;

// Every class fits two blocks in its span, and the header fits in slot 0.
const _: () = assert!(2 * crate::class::SMALL_MAX <= 1 << SMALL_SHIFT);
const _: () = assert!(2 * crate::class::MAX <= 1 << MEDIUM_SHIFT);
const _: () = assert!(size_of::<Segment>() <= 1 << SMALL_SHIFT);

/// Segments kept committed in the cache, ready to be reused as they are.
const HOT: usize = 8;

/// Segments kept reserved with their pages given back, past [`HOT`].
const COLD: usize = 64;

/// Large mappings kept for reuse.
const LARGE_KEEP: usize = 16;

/// The largest mapping the large cache keeps.
const LARGE_KEEP_MAX: usize = 64 << 20;

/// The most bytes of large mappings the cache keeps without giving their pages back.
const LARGE_HOT_BYTES: usize = 64 << 20;

/// What a header heads; the first byte of both headers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Kind {
    Small = 0,
    Medium = 1,
    Large = 2,
}

/// Where a span is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum State {
    /// On its heap's list of free spans, or never cut.
    Free = 0,
    /// On its class's list in its heap: it has a free block or room to cut one, or is the head.
    Bin = 1,
    /// Every block out: on no list until one comes back.
    Full = 2,
}

/// A span's descriptor, in its segment's header. Its owner alone reads and writes it, but for
/// `heap`, which a freeing thread reads to choose local or remote; `heap` changes only while
/// the span has no block out, when no thread may free into it.
#[repr(C)]
pub(crate) struct Span {
    /// Blocks freed back, newest first.
    pub(crate) free: *mut Block,
    /// The span's uncut rest: blocks are cut from it in order, `[bump, end)`.
    pub(crate) bump: *mut u8,
    pub(crate) end: *mut u8,
    /// The heap whose blocks these are; null while free.
    pub(crate) heap: *const Heap,
    /// Blocks out.
    pub(crate) used: u32,
    /// The block size.
    pub(crate) size: u32,
    /// Neighbours on its list (its class's, or the free spans').
    pub(crate) next: *mut Span,
    pub(crate) prev: *mut Span,
    pub(crate) class: u8,
    pub(crate) state: State,
    /// Its uncut rest reads zero: the span's memory came fresh from the system and was never
    /// written.
    pub(crate) fresh: bool,
    /// Its slot in its segment.
    pub(crate) slot: u8,
}

impl Span {
    pub(crate) const EMPTY: Span = Span {
        free: ptr::null_mut(),
        bump: ptr::null_mut(),
        end: ptr::null_mut(),
        heap: ptr::null(),
        used: 0,
        size: 0,
        next: ptr::null_mut(),
        prev: ptr::null_mut(),
        class: 0,
        state: State::Free,
        fresh: false,
        slot: 0,
    };
}

/// A small or medium segment's header, at the segment's base.
#[repr(C)]
pub(crate) struct Segment {
    pub(crate) kind: Kind,
    /// Its spans' size, as a shift.
    pub(crate) shift: u8,
    /// Its slots (spans plus the header's slot 0).
    pub(crate) slots: u8,
    /// Spans not free.
    pub(crate) used: u8,
    /// Its pages past the header were given back in the cache: [`Region::reuse`] them first.
    pub(crate) purged: bool,
    pub(crate) heap: *const Heap,
    /// Its link in a cache.
    pub(crate) next: *mut Segment,
    pub(crate) map: Mapping,
    pub(crate) spans: [Span; MAX_SPANS],
}

/// A large block's header, at the start of its block's first granule.
#[repr(C)]
pub(crate) struct Large {
    pub(crate) kind: Kind,
    /// Its block's bytes past `size` up to the committed end read zero, never written.
    pub(crate) fresh: bool,
    /// Its pages past the header page were given back in the cache.
    pub(crate) purged: bool,
    /// The heap its bytes count to ([`crate::Stats::large_bytes`]); any thread may free it.
    pub(crate) heap: *const Heap,
    /// The block's usable size and its offset from the mapping's base.
    pub(crate) size: usize,
    pub(crate) offset: usize,
    /// The header's offset from the mapping's base (0 but for a block aligned to 2 MiB).
    pub(crate) at: usize,
    pub(crate) map: Mapping,
}

/// The address of a block a caller handed back (to free, resize or size it): an address only.
/// It keeps no pointer and has no deref, so the caller's pointer, whose provenance covers its
/// own block and nothing else, can reach no metadata: [`header_of`] alone turns it into
/// pointers, and those carry the allocator's provenance over the whole mapping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Given(usize);

impl Given {
    /// The address of `block`; its provenance is dropped here.
    #[inline(always)]
    pub(crate) fn of<T>(block: *mut T) -> Given {
        Given(block.addr())
    }

    #[inline(always)]
    pub(crate) fn addr(self) -> usize {
        self.0
    }
}

/// What [`header_of`] finds for a block: its header, by a pointer formed from an address
/// (exposed provenance), used for one thing: to read the header's own mapping pointer. Every
/// pointer the allocator keeps or writes through ([`Found::small`], [`Found::large`]) is formed
/// from that mapping pointer (strict provenance), so nothing the caches or lists hold is an
/// address-made pointer, and Miri's leak check sees each kept mapping reachable.
#[derive(Clone, Copy)]
pub(crate) struct Found {
    header: *mut Kind,
    at: Given,
}

impl Found {
    /// The header's kind.
    ///
    /// # Safety
    ///
    /// The block is a live block of this allocator.
    #[inline(always)]
    pub(crate) unsafe fn kind(self) -> Kind {
        // SAFETY: a live block's header is live; its first byte is its kind.
        unsafe { *self.header }
    }

    /// A small or medium block's segment and the block itself, from the segment's mapping.
    ///
    /// # Safety
    ///
    /// As for [`Found::kind`], and the kind is not [`Kind::Large`].
    #[inline(always)]
    pub(crate) unsafe fn small(self) -> (*mut Segment, *mut u8) {
        // SAFETY: the caller's: a live segment header, its mapping live.
        let seg = unsafe { (*self.header.cast::<Segment>()).map.base() };
        let block = seg.wrapping_add(self.at.0 - seg.addr());
        (seg.cast(), block)
    }

    /// A large block's header, from its mapping.
    ///
    /// # Safety
    ///
    /// As for [`Found::kind`], and the kind is [`Kind::Large`].
    #[inline(always)]
    pub(crate) unsafe fn large(self) -> *mut Large {
        let large = self.header.cast::<Large>();
        // SAFETY: the caller's: a live large header, its mapping live.
        let (base, at) = unsafe { ((*large).map.base(), (*large).at) };
        base.wrapping_add(at).cast()
    }
}

/// The header of the block at `at`: its granule's start, or the granule before when the block
/// starts on a boundary (a large block aligned to 2 MiB or more).
///
/// The one place an address becomes a pointer. Each mapping's provenance is exposed once, when
/// it is reserved ([`Mapping::reserve`]), and the header pointer takes it back from the address
/// (`with_exposed_provenance_mut`): no load; masking the caller's pointer instead would be
/// undefined behaviour (it has no right to the header's bytes). The alternative, a page map from
/// address to the segment's pointer, keeps strict provenance at a dependent load on every free;
/// see docs/v2/plan/design/malloc.md.
#[inline(always)]
pub(crate) fn header_of(at: Given) -> Found {
    let off = at.0 & (SEG - 1);
    let back = if off == 0 { SEG } else { off };
    Found { header: ptr::with_exposed_provenance_mut(at.0 - back), at }
}

// ---------------------------------------------------------------------------------------------
// Mappings.

/// One reservation of the page source: a blaze-mem [`Region`] (reserved aligned to 2 MiB,
/// committed in a growing prefix), or under Miri, whose interpreter has no `mprotect` or
/// `madvise`, a zeroed block of the C library's `posix_memalign` aligned the same. Not the
/// global allocator: when this crate *is* the global allocator, that would call back into it
/// for every reservation, without end (the cause of a SIGKILL of the global-install test under
/// Miri, 2026-11-16).
pub(crate) struct Mapping {
    #[cfg(not(miri))]
    region: blaze_mem::Region,
    #[cfg(miri)]
    base: ptr::NonNull<u8>,
    #[cfg(miri)]
    bytes: usize,
}

impl Mapping {
    /// Reserves `bytes` (rounded up to the region's granule), aligned to [`SEG`].
    fn reserve(bytes: usize, pages: Pages) -> Result<Mapping, MallocError> {
        #[cfg(not(miri))]
        {
            let region = blaze_mem::Region::reserve_pages(bytes, pages, blaze_mem::Tag::Other)
                .map_err(MallocError::Os)?;
            debug_assert_eq!(region.base().as_ptr().addr() % SEG, 0, "a region aligned to 2 MiB");
            // Exposed once, for `header_of`.
            let _ = region.base().as_ptr().expose_provenance();
            Ok(Mapping { region })
        }
        #[cfg(miri)]
        {
            let _ = pages;
            let bytes = bytes.next_multiple_of(SEG);
            let mut base = ptr::null_mut();
            // SAFETY: `base` is a place for the result; SEG is a power of two and a multiple of
            // the pointer size.
            let code = unsafe { miri_libc::posix_memalign(&mut base, SEG, bytes) };
            let base = ptr::NonNull::new(base.cast::<u8>())
                .filter(|_| code == 0)
                .ok_or(MallocError::Os(MemError::TooLarge {
                    tag: blaze_mem::Tag::Other,
                    what: "a mapping under Miri",
                    wanted: bytes,
                }))?;
            // SAFETY: `bytes` bytes at `base` were just allocated for this mapping alone.
            unsafe { ptr::write_bytes(base.as_ptr(), 0, bytes) };
            // Exposed once, for `header_of`.
            let _ = base.as_ptr().expose_provenance();
            Ok(Mapping { base, bytes })
        }
    }

    #[inline(always)]
    pub(crate) fn base(&self) -> *mut u8 {
        #[cfg(not(miri))]
        return self.region.base().as_ptr();
        #[cfg(miri)]
        return self.base.as_ptr();
    }

    pub(crate) fn reserved(&self) -> usize {
        #[cfg(not(miri))]
        return self.region.reserved();
        #[cfg(miri)]
        return self.bytes;
    }

    fn committed(&self) -> usize {
        #[cfg(not(miri))]
        return self.region.committed();
        #[cfg(miri)]
        return self.bytes;
    }

    /// Makes `[base, base + len)` readable and writable.
    #[inline]
    pub(crate) fn ensure(&self, len: usize) -> Result<(), MallocError> {
        #[cfg(not(miri))]
        return self.region.ensure(len).map_err(MallocError::Os);
        #[cfg(miri)]
        return if len <= self.bytes {
            Ok(())
        } else {
            Err(MallocError::Os(MemError::Exhausted {
                tag: blaze_mem::Tag::Other,
                reserved: self.bytes,
                wanted: len,
            }))
        };
    }

    /// Gives the pages from `from` to the committed end back to the system.
    ///
    /// # Safety
    ///
    /// Nothing reads those bytes again before writing them, and nothing touches them before
    /// [`Mapping::reuse`] is called over them.
    unsafe fn purge_from(&self, from: usize) {
        #[cfg(not(miri))]
        {
            let len = self.region.committed().saturating_sub(from);
            // SAFETY: the caller's.
            let _ = unsafe { self.region.purge(from, len) };
        }
        #[cfg(miri)]
        let _ = from;
    }

    /// Makes the pages from `from` to the committed end, purged before, the process's again.
    fn reuse_from(&self, from: usize) -> Result<(), MallocError> {
        #[cfg(not(miri))]
        {
            let len = self.region.committed().saturating_sub(from);
            return self.region.reuse(from, len).map_err(MallocError::Os);
        }
        #[cfg(miri)]
        {
            let _ = from;
            Ok(())
        }
    }
}

#[cfg(miri)]
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `base` came from `posix_memalign`, and the mapping is gone, so nothing
        // reaches it.
        unsafe { miri_libc::free(self.base.as_ptr().cast()) };
    }
}

/// The C library's aligned allocation, which Miri's interpreter provides itself.
#[cfg(miri)]
mod miri_libc {
    use core::ffi::{c_int, c_void};

    unsafe extern "C" {
        pub(super) fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> c_int;
        pub(super) fn free(ptr: *mut c_void);
    }
}

/// Releases the mapping whose header `map` is a field of: moves it out of the memory it maps,
/// then drops it.
///
/// # Safety
///
/// `map` points at a live mapping inside the memory it maps, and nothing touches that memory
/// again.
unsafe fn release(map: *mut Mapping) {
    // SAFETY: the caller's: the value is live and read once, so it is dropped once.
    let map = unsafe { ptr::read(map) };
    drop(map);
}

// ---------------------------------------------------------------------------------------------
// The caches.

/// A value under a [`Spin`].
struct Locked<T> {
    spin: Spin,
    value: UnsafeCell<T>,
}

// SAFETY: the value is reached only inside `with`, under the lock, from one thread at a time;
// what it holds (raw pointers to headers) is plain data every thread may move.
unsafe impl<T> Sync for Locked<T> {}

impl<T> Locked<T> {
    const fn new(value: T) -> Locked<T> {
        Locked { spin: Spin::new(), value: UnsafeCell::new(value) }
    }

    fn with<R>(&self, body: impl FnOnce(&mut T) -> R) -> R {
        // SAFETY: the lock is held for the whole call, so this is the one reference.
        self.spin.with(|| body(unsafe { &mut *self.value.get() }))
    }
}

/// A stack of up to `N` headers.
struct Stack<T, const N: usize> {
    at: [*mut T; N],
    len: usize,
}

impl<T, const N: usize> Stack<T, N> {
    const fn new() -> Stack<T, N> {
        Stack { at: [ptr::null_mut(); N], len: 0 }
    }

    fn push(&mut self, value: *mut T) -> Result<(), *mut T> {
        if self.len == N {
            return Err(value);
        }
        self.at[self.len] = value;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<*mut T> {
        self.len = self.len.checked_sub(1)?;
        Some(self.at[self.len])
    }
}

struct Segments {
    hot: Stack<Segment, HOT>,
    cold: Stack<Segment, COLD>,
}

static SEGMENTS: Locked<Segments> =
    Locked::new(Segments { hot: Stack::new(), cold: Stack::new() });

struct Larges {
    kept: Stack<Large, LARGE_KEEP>,
    /// Bytes reserved by the kept mappings not purged.
    hot: usize,
}

static LARGES: Locked<Larges> = Locked::new(Larges { kept: Stack::new(), hot: 0 });

/// The pages large mappings are made of ([`set_large_pages`]): blaze-mem's [`Pages`] as a byte.
static LARGE_PAGES: AtomicU8 = AtomicU8::new(Pages::Base as u8);

/// Process-wide counts, moved on the cold paths only.
pub(crate) static SEGMENTS_HELD: AtomicU64 = AtomicU64::new(0);
pub(crate) static SEGMENTS_CACHED: AtomicU64 = AtomicU64::new(0);
pub(crate) static LARGE_HELD: AtomicU64 = AtomicU64::new(0);
pub(crate) static LARGE_CACHED: AtomicU64 = AtomicU64::new(0);

/// Makes large mappings (past 64 KiB) of `pages` from now on: on Linux, transparent huge pages
/// ([`Pages::Transparent`]) for the big blocks that are read at random. Segments stay on base
/// pages: a 2 MiB segment is one huge page at most, and its spans are touched sparsely.
pub fn set_large_pages(pages: Pages) {
    LARGE_PAGES.store(pages as u8, Ordering::Relaxed);
}

fn large_pages() -> Pages {
    match LARGE_PAGES.load(Ordering::Relaxed) {
        2 => Pages::Transparent,
        3 => Pages::Reserved,
        _ => Pages::Base,
    }
}

/// The bytes of a segment's header, rounded up to the system's page: what stays when its other
/// pages are given back.
fn header_pages() -> usize {
    size_of::<Segment>().next_multiple_of(blaze_mem::page_size())
}

/// A segment of `kind` for `heap`, its spans free and on no list: from the cache, or reserved.
pub(crate) fn acquire(kind: Kind, heap: *const Heap) -> Result<*mut Segment, MallocError> {
    let cached = SEGMENTS.with(|it| it.hot.pop().or_else(|| it.cold.pop()));
    let (seg, fresh) = match cached {
        Some(seg) => {
            SEGMENTS_CACHED.fetch_sub(1, Ordering::Relaxed);
            // SAFETY: a cached segment is a live header no heap holds; the cache gave it to us.
            if unsafe { (*seg).purged } {
                // SAFETY: as above.
                unsafe { (*seg).map.reuse_from(header_pages()) }?;
            }
            (seg, false)
        }
        None => {
            let map = Mapping::reserve(SEG, Pages::Base)?;
            map.ensure(size_of::<Segment>())?;
            let seg = map.base().cast::<Segment>();
            // SAFETY: the mapping's first bytes are committed (just above), aligned to 2 MiB, and
            // nothing else knows of them; the header moves the mapping into the memory it maps.
            unsafe { ptr::write(&raw mut (*seg).map, map) };
            SEGMENTS_HELD.fetch_add(1, Ordering::Relaxed);
            (seg, true)
        }
    };
    let shift = if kind == Kind::Small { SMALL_SHIFT } else { MEDIUM_SHIFT };
    let slots = (SEG >> shift) as u8;
    // SAFETY: `seg` is a header this thread alone holds (fresh, or popped from the cache); its
    // `map` was written and is kept; every other field is written here.
    unsafe {
        (*seg).kind = kind;
        (*seg).shift = shift as u8;
        (*seg).slots = slots;
        (*seg).used = 0;
        (*seg).purged = false;
        (*seg).heap = heap;
        (*seg).next = ptr::null_mut();
        for slot in 0..MAX_SPANS {
            let span = &raw mut (*seg).spans[slot];
            span.write(Span::EMPTY);
            (*span).slot = slot as u8;
            (*span).fresh = fresh;
        }
    }
    Ok(seg)
}

/// Gives back a segment none of whose spans is in use: to the cache, its pages given back when
/// it is past the hot ones, or released past the cold ones.
///
/// # Safety
///
/// `seg` is a live segment its heap has taken all its spans off its lists of, and nothing holds
/// a block in it.
pub(crate) unsafe fn retire(seg: *mut Segment) {
    let back = SEGMENTS.with(|it| it.hot.push(seg));
    let Err(seg) = back else {
        SEGMENTS_CACHED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // SAFETY: the caller says no block in it is held, so its pages past the header hold nothing
    // anyone reads; `purged` makes the next taker reuse them before writing.
    unsafe {
        (*seg).map.purge_from(header_pages());
        (*seg).purged = true;
    }
    match SEGMENTS.with(|it| it.cold.push(seg)) {
        Ok(()) => {
            SEGMENTS_CACHED.fetch_add(1, Ordering::Relaxed);
        }
        Err(seg) => {
            SEGMENTS_HELD.fetch_sub(1, Ordering::Relaxed);
            // SAFETY: the segment is on no list and holds no block; its mapping is its header's.
            unsafe { release(&raw mut (*seg).map) };
        }
    }
}

/// The start of span `slot` of `seg`.
#[inline(always)]
pub(crate) fn span_start(seg: *mut Segment, shift: u32, slot: usize) -> *mut u8 {
    seg.cast::<u8>().wrapping_add(slot << shift)
}

// ---------------------------------------------------------------------------------------------
// Large blocks.

/// The reservation for a large block needing `need` bytes from its mapping's base: twice its
/// power of two, so a block grown by doubling is grown in place once more before it moves; past
/// 1 GiB, just what it needs.
fn large_reservation(need: usize) -> usize {
    if need > 1 << 30 {
        need.next_multiple_of(SEG)
    } else {
        (need.next_power_of_two() * 2).max(SEG)
    }
}

/// A large block for `layout`, its bytes counted to `heap`. Zeroed when `zero` (a fresh mapping
/// reads zero already, so only a reused one is filled).
pub(crate) fn large_alloc(
    layout: Layout,
    heap: &Heap,
    zero: bool,
) -> Result<*mut u8, MallocError> {
    let (size, align) = (layout.size(), layout.align());
    let too_large = MallocError::TooLarge { size, align };
    if align >= SEG {
        return large_aligned(size, align, heap);
    }
    let offset = size_of::<Large>().next_multiple_of(align);
    let need = offset.checked_add(size).ok_or(too_large)?;
    // The smallest kept mapping that fits it and is at most four times its need.
    let kept = LARGES.with(|it| {
        let mut best: Option<usize> = None;
        for at in 0..it.kept.len {
            // SAFETY: a kept header is live and the cache's alone, under the lock.
            let reserved = unsafe { (*it.kept.at[at]).map.reserved() };
            let fits = reserved >= need && reserved <= need.saturating_mul(4).max(SEG);
            // SAFETY: as above.
            if fits && best.is_none_or(|b| reserved < unsafe { (*it.kept.at[b]).map.reserved() }) {
                best = Some(at);
            }
        }
        let at = best?;
        let large = it.kept.at[at];
        it.kept.len -= 1;
        it.kept.at[at] = it.kept.at[it.kept.len];
        // SAFETY: as above.
        if !unsafe { (*large).purged } {
            // SAFETY: as above.
            it.hot -= unsafe { (*large).map.reserved() };
        }
        Some(large)
    });
    let large = match kept {
        Some(large) => {
            LARGE_CACHED.fetch_sub(1, Ordering::Relaxed);
            // SAFETY: the cache gave this header to this thread alone.
            unsafe {
                if (*large).purged {
                    (*large).map.reuse_from(blaze_mem::page_size())?;
                    (*large).purged = false;
                }
                (*large).map.ensure(need)?;
                (*large).fresh = false;
            }
            large
        }
        None => {
            let map = Mapping::reserve(large_reservation(need), large_pages())?;
            map.ensure(need)?;
            let large = map.base().cast::<Large>();
            // SAFETY: the mapping's first bytes are committed and nothing else knows of them; the
            // header moves the mapping into the memory it maps.
            unsafe { ptr::write(&raw mut (*large).map, map) };
            // SAFETY: as above.
            unsafe { (*large).fresh = true };
            LARGE_HELD.fetch_add(1, Ordering::Relaxed);
            large
        }
    };
    // SAFETY: `large` is a header this thread alone holds, its map written above.
    unsafe {
        (*large).kind = Kind::Large;
        (*large).purged = false;
        (*large).heap = heap;
        (*large).size = size;
        (*large).offset = offset;
        (*large).at = 0;
    }
    heap.count_large(size, true);
    let block = large.cast::<u8>().wrapping_add(offset);
    // SAFETY: as above.
    if zero && !unsafe { (*large).fresh } {
        // SAFETY: `[block, block + size)` is committed (`ensure(need)`) and this thread's alone.
        unsafe { ptr::write_bytes(block, 0, size) };
    }
    Ok(block)
}

/// A large block aligned to 2 MiB or more: its block on a granule boundary, its header the
/// granule before. Never kept in the cache (its header is not at its mapping's base).
fn large_aligned(size: usize, align: usize, heap: &Heap) -> Result<*mut u8, MallocError> {
    let too_large = MallocError::TooLarge { size, align };
    let bytes = size.checked_add(align).and_then(|it| it.checked_add(SEG)).ok_or(too_large)?;
    let map = Mapping::reserve(bytes, large_pages())?;
    let base = map.base();
    // The first address past one granule from the base that is `align`-aligned.
    let offset = (base.addr() + SEG).next_multiple_of(align) - base.addr();
    let at = offset - SEG;
    map.ensure(offset + size)?;
    let large = base.wrapping_add(at).cast::<Large>();
    // SAFETY: `[base + at, base + offset + size)` is committed and nothing else knows of it; the
    // header moves the mapping into the memory it maps.
    unsafe {
        ptr::write(
            large,
            Large { kind: Kind::Large, fresh: true, purged: false, heap, size, offset, at, map },
        );
    }
    LARGE_HELD.fetch_add(1, Ordering::Relaxed);
    heap.count_large(size, true);
    Ok(base.wrapping_add(offset))
}

/// Frees the large block whose header is `large`: to the cache, or released.
///
/// # Safety
///
/// `large` heads a live large block that its caller gives up.
pub(crate) unsafe fn large_free(large: *mut Large) {
    // SAFETY: the caller's: a live header.
    let (heap, size, at, reserved) =
        unsafe { ((*large).heap, (*large).size, (*large).at, (*large).map.reserved()) };
    // SAFETY: the heap a large block counts to is a static slot, alive for the process.
    unsafe { (*heap).count_large(size, false) };
    if at == 0 && reserved <= LARGE_KEEP_MAX {
        let purge = LARGES.with(|it| it.hot + reserved > LARGE_HOT_BYTES);
        if purge {
            // SAFETY: the block is given up, so its bytes past the header page hold nothing
            // anyone reads; `purged` makes the next taker reuse them first.
            unsafe {
                (*large).map.purge_from(blaze_mem::page_size());
                (*large).purged = true;
            }
        }
        let kept = LARGES.with(|it| {
            let pushed = it.kept.push(large).is_ok();
            // SAFETY: under the lock, the header is the cache's (or still ours, refused).
            if pushed && !unsafe { (*large).purged } {
                it.hot += reserved;
            }
            pushed
        });
        if kept {
            LARGE_CACHED.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    LARGE_HELD.fetch_sub(1, Ordering::Relaxed);
    // SAFETY: the block is given up and on no list; its mapping is its header's.
    unsafe { release(&raw mut (*large).map) };
}

/// Grows or shrinks the large block at `block` (header `large`) to `size` in place, when its
/// mapping's reservation holds it: `true` if it did.
///
/// # Safety
///
/// `large` heads the live large block `block`, which its caller holds.
pub(crate) unsafe fn large_resize(large: *mut Large, size: usize) -> Result<bool, MallocError> {
    // SAFETY: the caller's.
    let (offset, old, reserved, heap) =
        unsafe { ((*large).offset, (*large).size, (*large).map.reserved(), (*large).heap) };
    let Some(need) = offset.checked_add(size) else { return Ok(false) };
    if need > reserved {
        return Ok(false);
    }
    // SAFETY: as above.
    unsafe { (*large).map.ensure(need) }?;
    // SAFETY: as above.
    unsafe { (*large).size = size };
    // SAFETY: the heap a large block counts to is a static slot.
    unsafe {
        (*heap).count_large(old, false);
        (*heap).count_large(size, true);
    }
    Ok(true)
}

/// The usable size of the large block headed by `large`.
///
/// # Safety
///
/// `large` heads a live large block.
pub(crate) unsafe fn large_size(large: *const Large) -> usize {
    // SAFETY: the caller's.
    unsafe { (*large).size }
}

/// Process-wide counts of the page source.
pub(crate) fn counts() -> (u64, u64, u64, u64) {
    (
        SEGMENTS_HELD.load(Ordering::Relaxed),
        SEGMENTS_CACHED.load(Ordering::Relaxed),
        LARGE_HELD.load(Ordering::Relaxed),
        LARGE_CACHED.load(Ordering::Relaxed),
    )
}

/// Releases every cached segment and large mapping: what a caller that wants the address space
/// back calls (the tests, before counting). Returns how many went.
pub fn trim() -> usize {
    let mut gone = 0;
    while let Some(seg) = SEGMENTS.with(|it| it.cold.pop().or_else(|| it.hot.pop())) {
        SEGMENTS_CACHED.fetch_sub(1, Ordering::Relaxed);
        SEGMENTS_HELD.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: a cached segment holds no block and is on no list but the cache's, which gave
        // it up.
        unsafe { release(&raw mut (*seg).map) };
        gone += 1;
    }
    while let Some(large) = LARGES.with(|it| {
        let large = it.kept.pop()?;
        // SAFETY: under the lock, the header is the cache's.
        if !unsafe { (*large).purged } {
            // SAFETY: as above.
            it.hot -= unsafe { (*large).map.reserved() };
        }
        Some(large)
    }) {
        LARGE_CACHED.fetch_sub(1, Ordering::Relaxed);
        LARGE_HELD.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: as for segments.
        unsafe { release(&raw mut (*large).map) };
        gone += 1;
    }
    gone
}
