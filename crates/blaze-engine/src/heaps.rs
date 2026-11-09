//! mimalloc with one heap per blaze-mem tag: the daemon's global allocator under
//! [`blaze_graph::arena::Counting`], which charges each allocation to the tag of the task
//! running on the thread and each free to the tag whose heap holds the block, so the memory line
//! names what holds the global heap by subsystem (`blaze_mem::tag::Counters::heap`).
//!
//! mimalloc 3's heaps are first-class: any thread allocates from one (through a thread-local
//! heap of its own, made on first use) and any thread frees into it, and a heap's blocks sit on
//! pages of their own, each page naming its heap. A block allocated outside any task is the main
//! heap's, as with plain mimalloc.
//!
//! A free finds the block's heap the way `mi_free` finds its page: two loads through mimalloc's
//! page map and one from the page ([`heap_of`], inlined; `mi_heap_of` is a call and checks its
//! address against the whole map), then the heap's tag in a table ([`holder`]). The page map's
//! shape and the page's layout are mimalloc's, mirrored here: the first tag heap made checks the
//! mirror against `mi_heap_of` ([`check_page_map`]) and stops the process, saying so, where they
//! differ, before any block is counted to a tag. Each tag's heap keeps partly used pages of its
//! own, so the heap holds a few pages more per tag and thread than one heap would.

#![allow(unsafe_code, reason = "mimalloc's heap calls, behind the global allocator")]

use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    ffi::c_void,
    ptr,
    sync::{
        Once,
        atomic::{AtomicPtr, AtomicUsize, Ordering},
    },
};

use blaze_graph::arena::{Holders, Malloc};
use blaze_mem::Tag;
use libmimalloc_sys::{
    mi_collect, mi_free, mi_heap_delete, mi_heap_malloc_aligned, mi_heap_new,
    mi_heap_realloc_aligned, mi_heap_t, mi_heap_zalloc_aligned, mi_is_in_heap_region, mi_malloc,
    mi_malloc_aligned, mi_process_info, mi_realloc_aligned, mi_theap_t, mi_usable_size, mi_zalloc,
    mi_zalloc_aligned,
};

unsafe extern "C" {
    /// The heap a block of mimalloc's is on; null for an address mimalloc does not own. What
    /// [`check_page_map`] holds [`heap_of`] to.
    fn mi_heap_of(p: *const c_void) -> *mut mi_heap_t;
    /// The main heap, which a page of its names as null.
    fn mi_heap_main() -> *mut mi_heap_t;
    /// The calling thread's part of `heap`, made on first use: what a thread allocates a heap's
    /// blocks from, its own until the thread ends.
    fn mi_heap_theap(heap: *mut mi_heap_t) -> *mut mi_theap_t;
    fn mi_theap_malloc(theap: *mut mi_theap_t, size: usize) -> *mut c_void;
    fn mi_theap_zalloc(theap: *mut mi_theap_t, size: usize) -> *mut c_void;
    fn mi_theap_malloc_aligned(
        theap: *mut mi_theap_t,
        size: usize,
        alignment: usize,
    ) -> *mut c_void;
    /// Collects `theap`, a theap of the calling thread's: takes in the blocks other threads freed
    /// into its pages, gives each page left empty back to its arena (`force`: retired ones too,
    /// and full pages visited), and purges the arenas' free pages now rather than after
    /// `purge_delay` (`force`). Only the theap's own thread may: its pages' local lists are that
    /// thread's alone.
    fn mi_theap_collect(theap: *mut mi_theap_t, force: bool);
    /// mimalloc 3's two-level page map (`page-map.c`; hidden, which a static link still
    /// resolves): for each 512 MiB of address space a sub-map, null until mimalloc maps pages
    /// there, of a page pointer per 64 KiB slice. Set once, when mimalloc starts, before its
    /// first block.
    static _mi_page_map: AtomicPtr<AtomicPtr<*const u8>>;
}

// mimalloc's page map, as `internal.h`'s `_mi_unchecked_ptr_page` reads it in a release build.
// A flat map (`MI_PAGE_MAP_FLAT`) is only for address spaces of 40 bits or fewer.
const _: () = assert!(usize::BITS == 64, "the page map's slice is 64 KiB only on 64-bit targets");
/// `MI_ARENA_SLICE_SHIFT`: 13 + `MI_SIZE_SHIFT` (3) on 64-bit targets.
const SLICE_SHIFT: u32 = 16;
/// `MI_PAGE_MAP_SUB_SHIFT`: slices per sub-map.
const SUB_SHIFT: u32 = 13;
/// Where `mi_page_t` keeps its heap: after `xthread_id`, `free`, the four counts and two flags
/// (8 bytes together), `local_free`, `xthread_free`, `block_size`, `page_start` and `theap`.
/// The free-list keys before `theap` exist only with `MI_SECURE` of 3 or more or `MI_DEBUG`
/// (libmimalloc-sys's `secure` and `debug` features), which blaze does not set.
const HEAP_AT: usize = 8 * 8;

/// The heap `ptr`, a live block of mimalloc's, is on: its page from the page map, and the
/// page's heap, as `mi_heap_of` gives it without checking the address, but null for the main
/// heap (`mi_page_heap` maps a page's null to `mi_heap_main()`), which is no tag's.
#[inline(always)]
fn heap_of(ptr: *const u8) -> *mut mi_heap_t {
    let slice = ptr.addr() >> SLICE_SHIFT;
    // SAFETY: mimalloc set the map before it made `ptr`, and a live block's slice has a sub-map
    // and a page in it; the page stays while the block lives, and `HEAP_AT` is its heap field
    // (`check_page_map`). Reads only.
    unsafe {
        let map = _mi_page_map.load(Ordering::Relaxed);
        let sub = (*map.add(slice >> SUB_SHIFT)).load(Ordering::Relaxed);
        let page = *sub.add(slice & ((1 << SUB_SHIFT) - 1));
        page.add(HEAP_AT).cast::<*mut mi_heap_t>().read()
    }
}

/// Holds [`heap_of`] to `mi_heap_of` on blocks of `heap`'s and the main heap's, of sizes from a
/// small page's to a huge one's, and stops the process where they differ: a libmimalloc-sys
/// whose page map or `mi_page_t` changed would count frees to the wrong tag.
#[cold]
fn check_page_map(heap: *mut mi_heap_t) {
    for size in [48, 1000, 64 << 10, 1 << 20, 64 << 20] {
        // SAFETY: a heap of ours and the main heap; each block is freed below, unused.
        let blocks =
            unsafe { [mi_heap_malloc_aligned(heap, size, 16), mi_malloc_aligned(size, 16)] };
        for block in blocks {
            if block.is_null() {
                continue;
            }
            // SAFETY: a live block of mimalloc's.
            let theirs = unsafe { mi_heap_of(block) };
            let ours = match heap_of(block.cast()) {
                // SAFETY: no argument; the main heap is mimalloc's from its start.
                main if main.is_null() => unsafe { mi_heap_main() },
                heap => heap,
            };
            // SAFETY: the block, freed once.
            unsafe { mi_free(block) };
            if ours != theirs {
                say(b"blaze: mimalloc's page map, as crates/blaze-engine/src/heaps.rs reads it (heap_of), names another heap for a block than mi_heap_of does: libmimalloc-sys's page map or mi_page_t changed, so frees cannot be counted to their tags\n");
                std::process::abort();
            }
        }
    }
}

/// mimalloc, a heap per tag ([`Holders`]).
pub(crate) struct Heaps;

/// Each tag's heap, made on its first allocation.
static HEAPS: [AtomicPtr<mi_heap_t>; Tag::COUNT] =
    [const { AtomicPtr::new(ptr::null_mut()) }; Tag::COUNT];

/// [`check_page_map`] ran: once, as the first tag heap is made.
static CHECKED: Once = Once::new();

/// The most alignment every block of mimalloc's has without asking: a word, as each size class
/// is a whole number of words from an aligned page start. A layout within it takes the plain
/// entry points, whose fast path serves every size; `_aligned` ones serve only small sizes
/// fast and send the rest down a generic aligned path.
const WORD: usize = align_of::<usize>();

thread_local! {
    /// This thread's theap of each tag's heap (`mi_heap_theap`), found on the thread's first
    /// allocation for the tag. mimalloc caches only the theap a thread used last, so a thread
    /// going from task to task of other tags would look its theap up again at each switch. A
    /// `const` thread-local without a destructor, so reading it allocates nothing; mimalloc
    /// frees a thread's theaps after the thread's thread-local destructors have run, the last
    /// code that could allocate on it.
    static THEAPS: [Cell<*mut mi_theap_t>; Tag::COUNT] = const { [const { Cell::new(ptr::null_mut()) }; Tag::COUNT] };
}

thread_local! {
    /// The last heap give-back ([`blaze_mem::tag::give_back_heap`]) this thread did: behind
    /// [`blaze_mem::tag::heap_give_backs`], it gives back its heaps' free pages ([`settle`]).
    /// `const`, without a destructor, as [`THEAPS`].
    static GIVEN: Cell<u64> = const { Cell::new(0) };
}

/// Gives back what this thread's heaps keep free if a give-back was asked since it last did:
/// one relaxed load and a thread-local compare otherwise. What blaze-mem calls as a pool worker
/// goes idle ([`blaze_mem::tag::settle_heap`]) and each allocation for a task checks.
#[inline]
fn settle() {
    let asked = blaze_mem::tag::heap_give_backs();
    if GIVEN.get() != asked {
        give_back_here(asked);
    }
}

/// Collects, forced, every theap this thread has of a tag's heap and its theap of the main heap:
/// the pages a theap allocates from and retires empty are its thread's alone, which no other
/// thread may collect, and a block another thread frees there is seen as free only once the
/// owner looks; the first forced collect also purges every arena's freed pages now rather than
/// `purge_delay` later (memcap5, coord-111). Allocates nothing: mimalloc gives pages back to its
/// arenas and purges them with system calls of its own.
#[cold]
#[inline(never)]
fn give_back_here(asked: u64) {
    GIVEN.set(asked);
    THEAPS.with(|theaps| {
        for slot in theaps {
            let theap = slot.get();
            if !theap.is_null() {
                // SAFETY: a theap this thread found for a heap of ours, never deleted, and so
                // this thread's own until it ends.
                unsafe { mi_theap_collect(theap, true) };
            }
        }
    });
    // SAFETY: collects the calling thread's theap of the main heap; no argument.
    unsafe { mi_collect(true) };
}

/// This thread's theap of `tag`'s heap, found now and kept in `slot`; null if mimalloc makes
/// none. Out of line, so the allocation's path saves no registers for it.
#[cold]
#[inline(never)]
fn theap_for(tag: Tag, slot: &Cell<*mut mi_theap_t>) -> *mut mi_theap_t {
    let Some(heap) = heap(tag) else { return ptr::null_mut() };
    // SAFETY: a heap of ours, never deleted.
    let theap = unsafe { mi_heap_theap(heap) };
    slot.set(theap);
    theap
}

/// Allocates `size` bytes aligned to `align` in `tag`'s heap from this thread's theap of it.
/// Not inlined, as blaze-graph's `with_local` is not: a job may wait on one thread and resume
/// on another, so each allocation reads the thread-local afresh (rule 27).
#[inline(never)]
fn alloc_in(tag: Tag, size: usize, align: usize, zeroed: bool) -> *mut u8 {
    settle();
    THEAPS.with(|theaps| {
        let slot = &theaps[tag.index()];
        let mut theap = slot.get();
        if theap.is_null() {
            theap = theap_for(tag, slot);
            if theap.is_null() {
                return no_heap();
            }
        }
        // SAFETY: this thread's theap of a live heap, and a size and alignment of a valid
        // layout. A zeroed block aligned past a word, which no theap call makes, comes from
        // the heap.
        unsafe {
            match (align <= WORD, zeroed) {
                (true, false) => mi_theap_malloc(theap, size).cast(),
                (true, true) => mi_theap_zalloc(theap, size).cast(),
                (false, false) => mi_theap_malloc_aligned(theap, size, align).cast(),
                (false, true) => match heap(tag) {
                    Some(heap) => mi_heap_zalloc_aligned(heap, size, align).cast(),
                    None => no_heap(),
                },
            }
        }
    })
}

/// From a heap to its tag, for [`holder`]: open addressing over [`SLOTS`] slots, a heap's
/// first slot by its address. A slot holds the heap's address with its tag, plus one, in the
/// top byte, so one compare-and-swap publishes both; 0 is an empty slot.
const SLOTS: usize = 64;
static FILED: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];

/// The bits of a slot that are the heap's address.
const ADDRESS: usize = (1 << 56) - 1;

const _: () = assert!(Tag::COUNT * 2 <= SLOTS, "the heap table stays at most half full");
const _: () = assert!(usize::BITS == 64, "a slot packs a 56-bit address and a tag");

#[inline(always)]
fn first_slot(heap: *mut mi_heap_t) -> usize {
    // Fibonacci hashing of the address, past the bits its alignment fixes.
    ((heap.addr() >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        >> (usize::BITS - SLOTS.trailing_zeros())) as usize
}

/// Says `heap` is `tag`'s in the table [`holder`] reads; `false` where its address does not
/// fit a slot, so no block of it could be named.
fn file(heap: *mut mi_heap_t, tag: Tag) -> bool {
    if heap.addr() & !ADDRESS != 0 {
        return false;
    }
    let entry = heap.addr() | (tag.index() + 1) << 56;
    let mut at = first_slot(heap);
    // The table is at most half full, so a free slot is within `SLOTS` steps.
    for _ in 0..SLOTS {
        if FILED[at].compare_exchange(0, entry, Ordering::Release, Ordering::Relaxed).is_ok() {
            return true;
        }
        at = (at + 1) % SLOTS;
    }
    false
}

/// `tag`'s heap, made now if it is not yet: `None` only if mimalloc can make no heap.
#[inline(always)]
fn heap(tag: Tag) -> Option<*mut mi_heap_t> {
    let heap = HEAPS[tag.index()].load(Ordering::Acquire);
    match heap.is_null() {
        false => Some(heap),
        true => make(tag),
    }
}

#[cold]
#[inline(never)]
fn make(tag: Tag) -> Option<*mut mi_heap_t> {
    // SAFETY: makes a heap; mimalloc allocates its record from its own main heap, never through
    // the global allocator, so this does not come back here.
    let made = unsafe { mi_heap_new() };
    if made.is_null() {
        return None;
    }
    // Before any heap is filed, so no free is taken off a tag by an unchecked read.
    CHECKED.call_once(|| check_page_map(made));
    match HEAPS[tag.index()].compare_exchange(
        ptr::null_mut(),
        made,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            if !file(made, tag) {
                unnamed(made);
            }
            blaze_mem::tag::heap_by_tag();
            blaze_mem::tag::heap_committed_by(committed);
            blaze_mem::tag::heap_gives_back_by(settle);
            Some(made)
        }
        Err(theirs) => {
            // Another thread made it first; ours has no block yet.
            // SAFETY: `made` is a heap of ours that nothing allocated from or knows of.
            unsafe { mi_heap_delete(made) };
            Some(theirs)
        }
    }
}

/// The tag whose heap holds `ptr`, a live block of mimalloc's: `None` for the main heap's.
/// Most heaps are found at their first slot: a multiply and a load past [`heap_of`].
#[inline(always)]
fn holder(ptr: *mut u8) -> Option<Tag> {
    let heap = heap_of(ptr);
    if heap.is_null() {
        return None;
    }
    let mut at = first_slot(heap);
    for _ in 0..SLOTS {
        // Relaxed: the entry is one word, address and tag together, and nothing else is read
        // through it.
        let entry = FILED[at].load(Ordering::Relaxed);
        if entry & ADDRESS == heap.addr() && entry != 0 {
            return Tag::ALL.get((entry >> 56) - 1).copied();
        }
        if entry == 0 {
            return None;
        }
        at = (at + 1) % SLOTS;
    }
    None
}

/// What mimalloc has committed from the system now, over every heap: its live blocks and the free
/// pages it keeps until it purges them (`mi_process_info`'s `current_commit`, its count of the
/// pages it commits and decommits, kept in every build). What the memory line sets the global
/// heap's live bytes against, and what a release's pressure counts of the heap
/// (`blaze_mem::tag::Snapshot::heap_footprint`). It allocates nothing.
fn committed() -> u64 {
    let mut current = 0usize;
    // SAFETY: every out-pointer but `current_commit` is null, which `mi_process_info` skips;
    // `current` is a live `usize` of ours.
    unsafe {
        mi_process_info(
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut current,
            ptr::null_mut(),
            ptr::null_mut(),
        );
    }
    current as u64
}

/// mimalloc could make no heap for a tag: said before the allocation fails, since the failure
/// alone would read as running out of memory.
#[cold]
fn no_heap() -> *mut u8 {
    say(b"blaze: mimalloc made no heap for a blaze-mem tag (mi_heap_new returned null), so this allocation fails\n");
    ptr::null_mut()
}

/// A tag's heap at an address past what a slot holds: its blocks would count to no tag, so
/// blaze stops, saying so, rather than name the global heap wrongly.
#[cold]
fn unnamed(_: *mut mi_heap_t) -> ! {
    say(b"blaze: a mimalloc heap for a blaze-mem tag lies above 2^56, past what blaze's heap table holds (crates/blaze-engine/src/heaps.rs)\n");
    std::process::abort()
}

/// Writes `said` to standard error without allocating.
fn say(said: &[u8]) {
    _ = std::io::Write::write_all(&mut std::io::stderr(), said);
}

// SAFETY: each call is mimalloc's for the same layout (its `_aligned` functions take any
// power-of-two alignment), in the main heap or a tag's; `mi_free` frees a block of any heap
// from any thread.
unsafe impl GlobalAlloc for Heaps {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: a size and alignment of a valid layout.
        unsafe {
            match layout.align() <= WORD {
                true => mi_malloc(layout.size()).cast(),
                false => mi_malloc_aligned(layout.size(), layout.align()).cast(),
            }
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: a size and alignment of a valid layout.
        unsafe {
            match layout.align() <= WORD {
                true => mi_zalloc(layout.size()).cast(),
                false => mi_zalloc_aligned(layout.size(), layout.align()).cast(),
            }
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: the caller says `ptr` is a live block of this allocator.
        unsafe { mi_free(ptr.cast()) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `realloc_for` outside any task.
        unsafe { self.realloc_for(None, ptr, layout, new_size) }
    }
}

impl Holders for Heaps {
    const BY_TAG: bool = true;

    #[inline]
    unsafe fn alloc_for(&self, tag: Tag, layout: Layout, zeroed: bool) -> *mut u8 {
        alloc_in(tag, layout.size(), layout.align(), zeroed)
    }

    #[inline]
    unsafe fn holder(&self, ptr: *mut u8) -> Option<Tag> {
        holder(ptr)
    }

    #[inline]
    unsafe fn realloc_for(
        &self,
        into: Option<Tag>,
        ptr: *mut u8,
        layout: Layout,
        new_size: usize,
    ) -> *mut u8 {
        // SAFETY: the caller upholds `realloc`'s contract. mimalloc keeps a block that still
        // fits where it is, on its heap; one that moves goes to `into`'s heap (never deleted)
        // or the thread's default one.
        unsafe {
            match into {
                Some(tag) => {
                    let Some(heap) = heap(tag) else { return no_heap() };
                    mi_heap_realloc_aligned(heap, ptr.cast(), new_size, layout.align()).cast()
                }
                None => mi_realloc_aligned(ptr.cast(), new_size, layout.align()).cast(),
            }
        }
    }
}

// What C's `malloc` family needs past the layout-carrying calls (`crate::malloc`): mimalloc
// finds a block's page, and with it its size and heap, from the address alone.
impl Malloc for Heaps {
    /// Whether mimalloc's page map has a page for `ptr` (`mi_is_in_heap_region`, mimalloc's
    /// checked read of it, which any address may be given; [`heap_of`] may only read a live
    /// block's): the macOS zone's `size`, which libmalloc asks of every block any zone frees.
    #[inline]
    fn owns(&self, ptr: *const u8) -> bool {
        // SAFETY: reads the page map, checking each level; any address.
        unsafe { mi_is_in_heap_region(ptr.cast()) }
    }

    #[inline]
    unsafe fn usable(&self, ptr: *mut u8) -> usize {
        // SAFETY: the caller says `ptr` is a live block of mimalloc's.
        unsafe { mi_usable_size(ptr.cast()) }
    }

    #[inline]
    unsafe fn free(&self, ptr: *mut u8) {
        // SAFETY: the caller says `ptr` is a live block of mimalloc's; `mi_free` frees a block
        // of any heap from any thread.
        unsafe { mi_free(ptr.cast()) }
    }

    #[inline]
    unsafe fn realloc_in(&self, into: Option<Tag>, ptr: *mut u8, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `realloc`'s contract; at a word's alignment mimalloc's
        // `_aligned` reallocs are its plain ones, as `realloc_for`'s are.
        unsafe {
            match into {
                Some(tag) => {
                    let Some(heap) = heap(tag) else { return no_heap() };
                    mi_heap_realloc_aligned(heap, ptr.cast(), new_size, WORD).cast()
                }
                None => mi_realloc_aligned(ptr.cast(), new_size, WORD).cast(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 16).expect("a valid layout")
    }

    /// A block made for a tag is found on that tag's heap from any thread, goes to the heap of
    /// the tag a realloc that moves it is for, and one made outside any task is no tag's.
    #[test]
    fn a_block_is_found_on_the_heap_of_the_tag_it_was_made_for() {
        let heaps = Heaps;
        // SAFETY: valid layouts; each block is freed once, with its layout.
        unsafe {
            let infer = heaps.alloc_for(Tag::Infer, layout(48), false);
            let query = heaps.alloc_for(Tag::Query, layout(4 << 20), true);
            let plain = heaps.alloc(layout(48));
            assert!(!infer.is_null() && !query.is_null() && !plain.is_null());
            assert_eq!(heaps.holder(infer), Some(Tag::Infer));
            assert_eq!(heaps.holder(query), Some(Tag::Query));
            assert_eq!(heaps.holder(plain), None);
            assert_eq!(*query.add(12345), 0, "zeroed");
            // Moves: 48 bytes to 64 KiB is past any in-place growth.
            let moved = heaps.realloc_for(Some(Tag::Mir), infer, layout(48), 64 << 10);
            assert_eq!(heaps.holder(moved), Some(Tag::Mir));
            let sent = Sent(moved, query);
            std::thread::spawn(move || {
                let (moved, query) = sent.parts();
                assert_eq!(Heaps.holder(moved), Some(Tag::Mir), "another thread finds it too");
                Heaps.dealloc(moved, layout(64 << 10));
                Heaps.dealloc(query, layout(4 << 20));
            })
            .join()
            .expect("the other thread frees both");
            heaps.dealloc(plain, layout(48));
        }
    }

    /// A free finds the heap of a block of any size and alignment from the page map as
    /// `mi_heap_of` does: blocks over-aligned past a page, and huge ones, sit at offsets in
    /// their pages that a small block's do not.
    #[test]
    fn the_page_map_names_each_blocks_heap_as_mimalloc_does() {
        let heaps = Heaps;
        for (size, align) in [
            (24, 8),
            (40, 16),
            (4096, 8),
            (3000, 4096),
            (100, 64 << 10),
            (5 << 20, 4 << 20),
            (70 << 20, 16),
        ] {
            let layout = Layout::from_size_align(size, align).expect("a valid layout");
            // SAFETY: a valid layout; the block is freed once, with it.
            unsafe {
                let block = heaps.alloc_for(Tag::Parse, layout, size % 2 == 0);
                assert!(
                    !block.is_null() && block.addr() % align == 0,
                    "{size} B aligned to {align}"
                );
                assert_eq!(heap_of(block), mi_heap_of(block.cast()), "{size} B aligned to {align}");
                assert_eq!(heaps.holder(block), Some(Tag::Parse), "{size} B aligned to {align}");
                heaps.dealloc(block, layout);
            }
        }
    }

    /// What a thread freed of a tag's heap mimalloc keeps, its pages resident until a purge
    /// after `purge_delay` (1 s) that runs only as some thread next takes a page; a give-back
    /// purges it now, the freeing thread's theaps collected as it settles, so the process's
    /// footprint drops by it at once. Without the give-back the footprint keeps the pages.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_give_back_purges_the_pages_a_thread_freed_at_once() {
        const BLOCK: usize = 4 << 10;
        const BLOCKS: usize = 64 << 10; // 256 MiB
        let (freed, asked, settled) =
            (std::sync::Barrier::new(2), std::sync::Barrier::new(2), std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut blocks = Vec::with_capacity(BLOCKS);
                for _ in 0..BLOCKS {
                    // SAFETY: a valid layout; each block is freed below, once, with it.
                    let block = unsafe { Heaps.alloc_for(Tag::Parse, layout(BLOCK), false) };
                    assert!(!block.is_null());
                    // Touched, so its page is resident.
                    // SAFETY: the block is `BLOCK` bytes, ours.
                    unsafe { block.write_bytes(1, BLOCK) };
                    blocks.push(block);
                }
                for block in blocks {
                    // SAFETY: a live block made above with this layout, freed once.
                    unsafe { Heaps.dealloc(block, layout(BLOCK)) };
                }
                freed.wait();
                asked.wait();
                // As a pool worker going idle does.
                blaze_mem::tag::settle_heap();
                settled.wait();
            });
            freed.wait();
            let before =
                blaze_cargo::raw::memory::own_footprint().expect("the footprint is readable");
            blaze_mem::tag::give_back_heap();
            asked.wait();
            settled.wait();
            let after =
                blaze_cargo::raw::memory::own_footprint().expect("the footprint is readable");
            let given = before.saturating_sub(after);
            assert!(
                given >= (BLOCK * BLOCKS / 2) as u64,
                "the give-back took the footprint from {before} to {after}: {given} bytes of the {} the freed blocks held",
                BLOCK * BLOCKS,
            );
        });
    }

    /// Two blocks handed to another thread to free.
    struct Sent(*mut u8, *mut u8);

    impl Sent {
        /// Taken whole, so a closure moves the `Send` pair, not its fields.
        fn parts(self) -> (*mut u8, *mut u8) {
            (self.0, self.1)
        }
    }
    // SAFETY: the blocks are mimalloc's, which any thread may free; the sender keeps no use.
    unsafe impl Send for Sent {}
}
