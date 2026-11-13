//! mimalloc with one heap per blaze-mem tag, in an arena of blaze-mem's: the daemon's global
//! allocator under [`blaze_graph::arena::Counting`], which charges each allocation to the tag of
//! the task running on the thread and each free to the tag whose heap holds the block, so the
//! memory line names what holds the global heap by subsystem (`blaze_mem::tag::Counters::heap`).
//!
//! **The arena.** Every page mimalloc uses is in one sparse [`Region`] ([`Arena`]): 512 GiB of
//! address space reserved at the first allocation, handed to mimalloc in sub-arenas of 4 GiB
//! as it fills (`mi_manage_memory`, one more once less than 2 GiB of what it was given is
//! free), each committed and decommitted by the 64 KiB slice through [`commit`]: a slice
//! mimalloc commits is made the process's with [`Region::reuse`], which asks the memory gate's
//! core first (`blaze_mem::gate`), and one it purges goes back with [`Region::evict`]. mimalloc
//! may take no memory from the system past it (`disallow_os_alloc`), so an allocation the arena
//! cannot serve, or the gate refuses, is the typed [`ArenaFull`], said where it happens, never a
//! silent fall to `mmap`. What mimalloc holds is then exactly what the gate counts:
//! [`committed_now`] is the region's held bytes. The floors: a commit or purge costs the
//! kernel's page call per run of slices (macOS `MADV_FREE_REUSE` / `MADV_FREE_REUSABLE`, Linux
//! nothing / `MADV_DONTNEED`) plus one atomic word per 64 slices and the gate's one
//! compare-and-swap; mimalloc's own metadata is about 640 KiB a sub-arena (its slice bitmaps)
//! and the same again for each heap that uses one (its page bitmaps, `mi_arena_pages_alloc`),
//! committed as used. A block is at most 2 GiB (`arena_max_object_size`; a sub-arena holds no
//! more than 4 GiB) and aligned at most 64 KiB inside mimalloc; a Rust block aligned past that
//! is over-allocated here ([`over_aligned`]), and C's is refused (`crate::malloc`).
//!
//! **Heaps.** mimalloc 3's heaps are first-class: any thread allocates from one (through a
//! thread-local heap of its own, made on first use) and any thread frees into it, and a heap's
//! blocks sit on pages of their own, each page naming its heap. A block allocated outside any
//! task is the main heap's, as with plain mimalloc. Each tag has a heap that lives for the
//! process; a task that opts in ([`blaze_graph::arena::heap_dies_with_task`]) has one of its own
//! from the task-heap pool ([`open_task_heap`]), destroyed whole as it ends
//! ([`close_task_heap`]), its pages back to the arena and purged at once, whatever blocks it
//! left.
//!
//! **A value kept past its task.** A debug or test build quarantines a destroyed task heap's
//! pages: as they are purged they are made inaccessible (`mprotect(PROT_NONE)`), until mimalloc
//! commits them again for another use, so a read or free of a block kept past its task faults,
//! and [`kept_past_task`] names the task's tag at the faulting address. A release build destroys
//! the heap and does no more: sound only under the opt-in's contract, that the task's results
//! are records or bytes in buffers its caller made (a caller's block grown by the task stays in
//! the caller's heap, `Counting::moved_from`), so nothing reaches a block of the heap after it.
//!
//! A free finds the block's heap the way `mi_free` finds its page: two loads through mimalloc's
//! page map and one from the page ([`heap_of`], inlined; `mi_heap_of` is a call and checks its
//! address against the whole map), then the heap's holder in a table ([`holder`]). The page
//! map's shape and the page's layout are mimalloc's, mirrored here: the first tag heap made
//! checks the mirror against `mi_heap_of` ([`check_page_map`]) and stops the process, saying so,
//! where they differ, before any block is counted to a tag.

#![allow(unsafe_code, reason = "mimalloc's heap calls, behind the global allocator")]

use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    ffi::{c_char, c_int, c_long, c_void},
    fmt, ptr,
    sync::{
        Once, OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
};

use blaze_graph::arena::{
    Holder, Holders, MAX_TASK_HEAPS, Malloc, TaskHeap, TaskHeapRefused, TaskHeaps,
};
use blaze_mem::{MemError, Region, Tag};
use libmimalloc_sys::{
    mi_collect, mi_free, mi_heap_delete, mi_heap_destroy, mi_heap_malloc_aligned, mi_heap_new,
    mi_heap_realloc_aligned, mi_heap_t, mi_heap_zalloc_aligned, mi_is_in_heap_region, mi_malloc,
    mi_malloc_aligned, mi_option_set, mi_option_t, mi_realloc_aligned, mi_theap_t,
    mi_usable_size, mi_zalloc, mi_zalloc_aligned,
};

unsafe extern "C" {
    /// The heap a block of mimalloc's is on; null for an address mimalloc does not own. What
    /// [`check_page_map`] holds [`heap_of`] to.
    fn mi_heap_of(p: *const c_void) -> *mut mi_heap_t;
    /// The main heap, which a page of its names as null.
    fn mi_heap_main() -> *mut mi_heap_t;
    /// The calling thread's part of `heap`, made on first use: what a thread allocates a heap's
    /// blocks from, its own until the thread ends or the heap is destroyed.
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
    /// Gives mimalloc `[start, start + size)` to allocate in, as sub-arenas of at most 16 GiB
    /// (include/mimalloc.h; libmimalloc-sys 0.1.49 does not declare it): uncommitted
    /// (`is_committed` false), so every commit and decommit of it goes through `commit_fun`, and
    /// zero (`is_zero`), so mimalloc clears none of it. `exclusive`: only a heap made in it
    /// (`mi_heap_new_in_arena`) allocates there.
    fn mi_manage_memory(
        start: *mut c_void,
        size: usize,
        is_committed: bool,
        is_pinned: bool,
        is_zero: bool,
        numa_node: c_int,
        exclusive: bool,
        commit_fun: Option<CommitFun>,
        commit_fun_arg: *mut c_void,
        arena_id: *mut *mut c_void,
    ) -> bool;
    /// A heap that allocates only in the exclusive arena `arena_id`.
    fn mi_heap_new_in_arena(arena_id: *mut c_void) -> *mut mi_heap_t;
    /// Writes each option's name and value through `out`, one line each.
    fn mi_options_print_out(out: unsafe extern "C" fn(*const c_char, *mut c_void), arg: *mut c_void);
    /// Changes the protection of whole pages (POSIX).
    fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
}

/// `mi_commit_fun_t`: commits (`commit`) or decommits `[start, start + size)` of an arena; on a
/// commit, says through `is_zero` (when not null) whether the bytes read zero. Whether it did.
type CommitFun = unsafe extern "C" fn(bool, *mut c_void, usize, *mut bool, *mut c_void) -> bool;

// mimalloc's page map, as `internal.h`'s `_mi_unchecked_ptr_page` reads it in a release build.
// A flat map (`MI_PAGE_MAP_FLAT`) is only for address spaces of 40 bits or fewer.
const _: () = assert!(usize::BITS == 64, "the page map's slice is 64 KiB only on 64-bit targets");
/// `MI_ARENA_SLICE_SHIFT`: 13 + `MI_SIZE_SHIFT` (3) on 64-bit targets.
const SLICE_SHIFT: u32 = 16;
/// mimalloc's arena slice: the unit it commits and purges in, and so the arena's.
const SLICE: usize = 1 << SLICE_SHIFT;
/// `MI_PAGE_MAP_SUB_SHIFT`: slices per sub-map.
const SUB_SHIFT: u32 = 13;
/// Where `mi_page_t` keeps its heap: after `xthread_id`, `free`, the four counts and two flags
/// (8 bytes together), `local_free`, `xthread_free`, `block_size`, `page_start` and `theap`.
/// The free-list keys before `theap` exist only with `MI_SECURE` of 3 or more or `MI_DEBUG`
/// (libmimalloc-sys's `secure` and `debug` features), which blaze does not set.
const HEAP_AT: usize = 8 * 8;

/// The address space the arena reserves: past every heap blaze has measured (tempo's cold
/// build peaked at 110 GiB of process), and well inside `MI_MAX_ARENAS` (160) sub-arenas.
const RESERVE: usize = 512 << 30;
/// What one `mi_manage_memory` gives mimalloc: a sub-arena. mimalloc keeps a page bitmap of
/// 76 bits a slice per heap and sub-arena it uses (`mi_arena_pages_alloc`), about 600 KiB here,
/// and a sub-arena's slice bitmaps take as much of it; 16 GiB would make both four times that.
const SUB_ARENA: usize = 4 << 30;
/// The arena gives mimalloc its next sub-arena once less than this of what it has is free: so
/// there is room for the largest block while the next one is registered.
const GROW_UNDER: usize = 2 << 30;
/// The largest block mimalloc puts in the arena (`arena_max_object_size`, its default): one
/// larger would go to the system, which the arena disallows, so it is [`ArenaFull`].
const MAX_OBJECT: usize = 2 << 30;
/// `MI_PAGE_MAX_OVERALLOC_ALIGN`: past this alignment mimalloc maps a block from the system
/// (alloc-aligned.c), so [`over_aligned`] makes such blocks here instead.
pub(crate) const MAX_ALIGN: usize = 64 << 10;

const _: () = assert!(MAX_OBJECT < SUB_ARENA - (32 << 20), "a largest block fits a sub-arena");
const _: () = assert!(RESERVE / SUB_ARENA < 150, "the sub-arenas stay inside MI_MAX_ARENAS");

/// The main arena's committed slices, a bit each: 1 MiB, in `.bss`.
static MAIN_SLICES: [AtomicU64; RESERVE / SLICE / 64] =
    [const { AtomicU64::new(0) }; RESERVE / SLICE / 64];

/// The arena every heap of the process allocates in ([`start`]).
static MAIN: OnceLock<Arena> = OnceLock::new();

/// `mi_option_e`'s numbers (include/mimalloc.h), which libmimalloc-sys 0.1.49 names only in
/// part; `the_options_are_the_ones_this_names` reads them back by name.
const PURGE_DELAY: mi_option_t = 15;
const DISALLOW_OS_ALLOC: mi_option_t = 17;
const PAGE_COMMIT_ON_DEMAND: mi_option_t = 40;
const ARENA_MAX_OBJECT_SIZE: mi_option_t = 45;

/// A region of blaze-mem's that mimalloc allocates in: its slices, a bit each, say which are
/// committed, so each byte is made the process's once and given back once.
struct Arena {
    region: Region,
    /// The region's first slice-aligned address: slice 0.
    start: usize,
    /// Bytes from `start` mimalloc may be given: the reservation, in whole slices.
    usable: usize,
    /// Bytes from `start` given to mimalloc so far.
    registered: AtomicUsize,
    committed: &'static [AtomicU64],
    /// Gives mimalloc more as it fills ([`Arena::grow_if_low`]); a test's fixed arena does not.
    grows: bool,
    exclusive: bool,
    growing: AtomicBool,
    /// The first sub-arena's id, for an exclusive arena's heap.
    id: AtomicPtr<c_void>,
    /// Quarantines a destroyed task heap's pages in a debug or test build (the main arena's).
    #[cfg_attr(not(debug_assertions), allow(dead_code, reason = "a release build quarantines nothing"))]
    #[cfg_attr(
        not(debug_assertions),
        expect(blaze::unwired, reason = "wired in debug builds; a release build quarantines nothing")
    )]
    quarantine: bool,
}

thread_local! {
    /// What the gate (or the kernel) refused the last commit on this thread: its bytes and the
    /// system's error number, -1 for the gate's refusal; `(0, 0)` for none. Set by [`commit`],
    /// read by [`arena_full`] on the same thread, as mimalloc commits on the allocating
    /// thread. `const`, without a destructor.
    static REFUSED: Cell<(usize, i32)> = const { Cell::new((0, 0)) };
}

impl Arena {
    fn new(region: Region, committed: &'static [AtomicU64], grows: bool, exclusive: bool) -> Arena {
        let base = region.base().as_ptr().addr();
        let start = base.next_multiple_of(SLICE);
        let usable = ((region.reserved() - (start - base)) & !(SLICE - 1))
            .min(committed.len() * 64 * SLICE);
        Arena {
            region,
            start,
            usable,
            registered: AtomicUsize::new(0),
            committed,
            grows,
            exclusive,
            growing: AtomicBool::new(false),
            id: AtomicPtr::new(ptr::null_mut()),
            quarantine: grows,
        }
    }

    /// The region's offset of slice `slice`.
    fn offset(&self, slice: usize) -> usize {
        self.start - self.region.base().as_ptr().addr() + slice * SLICE
    }

    /// Whether `addr` is in what mimalloc was given.
    #[cfg_attr(
        not(any(test, debug_assertions)),
        allow(dead_code, reason = "the quarantine's and the tests'")
    )]
    #[cfg_attr(
        not(any(test, debug_assertions)),
        expect(blaze::unwired, reason = "wired in debug builds, by the quarantine")
    )]
    fn contains(&self, addr: usize) -> bool {
        addr >= self.start && addr - self.start < self.registered.load(Ordering::Acquire)
    }

    /// Gives mimalloc the next sub-arena, from `at` bytes past `start`. Whether it took it.
    fn register(&self, at: usize) -> bool {
        let size = SUB_ARENA.min(self.usable.saturating_sub(at)) & !((32 << 20) - 1);
        if size == 0 {
            return false;
        }
        let mut id = ptr::null_mut();
        let start = self.region.base().as_ptr().wrapping_add(self.offset(at >> SLICE_SHIFT));
        // SAFETY: `[start, start + size)` is whole slices of this live region, given to no one
        // else, and never committed (so zero); the arena lives for the process (a static, or a
        // test's leaked one), so the `commit` argument stays valid for every call mimalloc makes.
        let took = unsafe {
            mi_manage_memory(
                start.cast(),
                size,
                false,
                false,
                true,
                -1,
                self.exclusive,
                Some(commit),
                ptr::from_ref(self).cast_mut().cast(),
                &raw mut id,
            )
        };
        if took {
            if at == 0 {
                self.id.store(id, Ordering::Release);
            }
            self.registered.fetch_add(size, Ordering::AcqRel);
        }
        took
    }

    /// Registers the next sub-arena once less than [`GROW_UNDER`] of what mimalloc has is
    /// free: from inside a commit, the one place the arena sees mimalloc fill it. One thread
    /// registers; the others go on in what is there.
    fn grow_if_low(&self) {
        let low = |registered: usize| {
            registered < self.usable && self.region.held() + GROW_UNDER >= registered
        };
        if !low(self.registered.load(Ordering::Acquire))
            || self.growing.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let registered = self.registered.load(Ordering::Acquire);
        if low(registered) && !self.register(registered) {
            say(b"blaze: mimalloc refused the global heap's next sub-arena (mi_manage_memory, crates/blaze-engine/src/heaps.rs); allocations past what it has fail, each naming itself\n");
        }
        self.growing.store(false, Ordering::Release);
    }

    /// Commits the slices of `[addr, addr + size)`, rounded out: each run of them not committed
    /// yet is made the process's through the gate ([`Region::reuse`]). On a refusal the run
    /// refused, and the rest of the call's, are left uncommitted and the call fails; the runs
    /// before it stay committed and counted, which mimalloc's next commit of them finds.
    fn commit(&self, addr: usize, size: usize, is_zero: *mut bool) -> bool {
        let first = (addr - self.start) >> SLICE_SHIFT;
        let end = (addr + size - self.start).div_ceil(SLICE);
        #[cfg(debug_assertions)]
        if self.quarantine {
            quarantine::release(self, first, end);
        }
        let mut fresh = true;
        let mut pending: Option<(usize, usize)> = None;
        for word in first / 64..end.div_ceil(64) {
            let mask = word_mask(word, first, end);
            let was = self.committed[word].fetch_or(mask, Ordering::AcqRel);
            let mut new = mask & !was;
            fresh &= new == mask;
            while new != 0 {
                let lo = new.trailing_zeros() as usize;
                let len = (!(new >> lo)).trailing_zeros() as usize;
                let run = (word * 64 + lo, word * 64 + lo + len);
                new &= !span(lo, len);
                pending = match pending {
                    Some((from, to)) if to == run.0 => Some((from, run.1)),
                    Some((from, to)) => {
                        if !self.reuse(from, to) {
                            self.clear(from, to);
                            self.committed[word].fetch_and(!(new | span(lo, len)), Ordering::AcqRel);
                            return false;
                        }
                        Some(run)
                    }
                    None => Some(run),
                };
            }
        }
        if let Some((from, to)) = pending
            && !self.reuse(from, to)
        {
            self.clear(from, to);
            return false;
        }
        if !is_zero.is_null() {
            // Linux reads zeros in a page never touched or given back with `MADV_DONTNEED`;
            // macOS's `MADV_FREE_REUSABLE` leaves a page's bytes unspecified.
            // SAFETY: mimalloc's out-pointer for this call.
            unsafe { *is_zero = cfg!(target_os = "linux") && fresh };
        }
        if self.grows {
            self.grow_if_low();
        }
        true
    }

    /// Makes slices `[from, to)` the process's through the gate; says what refused it.
    fn reuse(&self, from: usize, to: usize) -> bool {
        match self.region.reuse(self.offset(from), (to - from) * SLICE) {
            Ok(()) => true,
            Err(error) => {
                let os = match &error {
                    MemError::Commit { os, .. } => os.raw_os_error().unwrap_or(-1),
                    _ => -1,
                };
                REFUSED.set(((to - from) * SLICE, os));
                false
            }
        }
    }

    /// Clears the committed bits of slices `[from, to)`.
    fn clear(&self, from: usize, to: usize) {
        for word in from / 64..to.div_ceil(64) {
            self.committed[word].fetch_and(!word_mask(word, from, to), Ordering::AcqRel);
        }
    }

    /// Gives back the committed slices of `[addr, addr + size)`, rounded in. Whether mimalloc
    /// must commit before reusing the range: yes whenever a slice of it is whole; a run the
    /// kernel would not drop stays committed and counted, which that commit finds.
    fn decommit(&self, addr: usize, size: usize) -> bool {
        let first = (addr - self.start).div_ceil(SLICE);
        let end = (addr + size - self.start) >> SLICE_SHIFT;
        if first >= end {
            return false;
        }
        let mut pending: Option<(usize, usize)> = None;
        for word in first / 64..end.div_ceil(64) {
            let mask = word_mask(word, first, end);
            let mut gone = mask & self.committed[word].fetch_and(!mask, Ordering::AcqRel);
            while gone != 0 {
                let lo = gone.trailing_zeros() as usize;
                let len = (!(gone >> lo)).trailing_zeros() as usize;
                let run = (word * 64 + lo, word * 64 + lo + len);
                gone &= !span(lo, len);
                pending = match pending {
                    Some((from, to)) if to == run.0 => Some((from, run.1)),
                    Some((from, to)) => {
                        self.evict(from, to);
                        Some(run)
                    }
                    None => Some(run),
                };
            }
        }
        if let Some((from, to)) = pending {
            self.evict(from, to);
        }
        true
    }

    /// Gives slices `[from, to)` back to the kernel, their bits already cleared.
    fn evict(&self, from: usize, to: usize) {
        let (at, len) = (self.offset(from), (to - from) * SLICE);
        // SAFETY: mimalloc purges only free slices, which nothing references; each was made the
        // process's by `reuse` (its bit was set) and not given back since.
        match unsafe { self.region.evict(&[at..at + len]) } {
            Ok(given) if given == len => {
                #[cfg(debug_assertions)]
                if self.quarantine {
                    quarantine::guard(self, from, to);
                }
            }
            // The kernel kept them: still the process's, so still committed and counted.
            Ok(_) => {
                for word in from / 64..to.div_ceil(64) {
                    self.committed[word].fetch_or(word_mask(word, from, to), Ordering::AcqRel);
                }
            }
            Err(_) => {
                say(b"blaze: this system cannot give the global heap's pages back (Region::evict refused: on Linux a kernel without process_madvise), so the heap could only grow; blaze stops (crates/blaze-engine/src/heaps.rs)\n");
                std::process::abort()
            }
        }
    }
}

/// The bits of `[lo, lo + len)` of a word.
fn span(lo: usize, len: usize) -> u64 {
    match len {
        64 => !0,
        len => ((1u64 << len) - 1) << lo,
    }
}

/// The bits of word `word` that are slices of `[first, end)`.
fn word_mask(word: usize, first: usize, end: usize) -> u64 {
    let lo = first.max(word * 64) - word * 64;
    let hi = end.min(word * 64 + 64) - word * 64;
    span(lo, hi - lo)
}

/// mimalloc's commit and decommit of an arena's slices ([`Arena::commit`], [`Arena::decommit`]).
unsafe extern "C" fn commit(
    commit: bool,
    start: *mut c_void,
    size: usize,
    is_zero: *mut bool,
    arena: *mut c_void,
) -> bool {
    // SAFETY: the argument `Arena::register` gave mimalloc: an arena that lives for the process.
    let arena = unsafe { &*arena.cast::<Arena>() };
    match commit {
        true => arena.commit(start.addr(), size, is_zero),
        false => arena.decommit(start.addr(), size),
    }
}

static READY: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// This thread is in [`start`]: what it allocates meanwhile (the process's memory gate,
    /// made on first use, reads the machine with `std`) comes from [`boot`]. `const`, without a
    /// destructor.
    static STARTING: Cell<bool> = const { Cell::new(false) };
}

/// Sets the arena up before mimalloc's first block: one relaxed load after the first call.
/// `false` only on the thread setting it up, from inside: that allocation is [`boot`]'s.
#[inline(always)]
fn ready() -> bool {
    READY.load(Ordering::Relaxed) || start()
}

/// Sets mimalloc's options, reserves the arena and gives mimalloc its first sub-arena, and says
/// how the task-heap pool opens and closes heaps; whether the arena is ready (`false` when
/// called again from inside, on the thread doing it). mimalloc's options and arena records are
/// its own; the region's gate, made on its first use, allocates, from [`boot`].
#[cold]
#[inline(never)]
fn start() -> bool {
    static START: Once = Once::new();
    if STARTING.get() {
        return false;
    }
    STARTING.set(true);
    START.call_once(|| {
        // SAFETY: option words, which mimalloc documents as settable from any thread at any
        // time; set before its first block.
        unsafe {
            mi_option_set(DISALLOW_OS_ALLOC, 1);
            // Option 40's on-demand commit commits a page's tail outside `commit_fun`
            // (page.c), which would pass the gate by.
            mi_option_set(PAGE_COMMIT_ON_DEMAND, 0);
            mi_option_set(ARENA_MAX_OBJECT_SIZE, (MAX_OBJECT >> 10) as c_long);
            mi_option_set(PURGE_DELAY, 0);
        }
        let region = match Region::reserve_sparse(RESERVE + SLICE, Tag::Other) {
            Ok(region) => region,
            Err(error) => {
                let os = match &error {
                    MemError::Reserve { os, .. } => os.raw_os_error().unwrap_or(-1),
                    _ => -1,
                };
                _ = std::io::Write::write_fmt(
                    &mut std::io::stderr(),
                    format_args!(
                        "blaze: the global heap could not reserve its arena of {RESERVE} bytes of \
                         address space (os error {os}); blaze stops (crates/blaze-engine/src/heaps.rs)\n"
                    ),
                );
                std::process::abort()
            }
        };
        let arena = MAIN.get_or_init(|| Arena::new(region, &MAIN_SLICES, true, false));
        if !arena.register(0) {
            say(b"blaze: mimalloc refused the global heap's first sub-arena (mi_manage_memory); blaze stops (crates/blaze-engine/src/heaps.rs)\n");
            std::process::abort();
        }
        blaze_graph::arena::task_heaps_by(TaskHeaps { open: open_task_heap, close: close_task_heap });
        READY.store(true, Ordering::Release);
    });
    STARTING.set(false);
    true
}

/// What the thread in [`start`] allocates before the arena is there: a bump of 1 MiB of
/// `.bss`, touched only as used, never reused. Its blocks are no tag's and freeing one does
/// nothing ([`boot::contains`], checked before mimalloc's page map on each free: one subtract
/// and compare). A start that needs more stops, saying so.
mod boot {
    use std::{
        alloc::Layout,
        cell::UnsafeCell,
        sync::atomic::{AtomicUsize, Ordering},
    };

    const BYTES: usize = 1 << 20;

    #[repr(C, align(65536))]
    struct Buffer(UnsafeCell<[u8; BYTES]>);

    // SAFETY: each byte is handed out once, by the bump below, to one block.
    unsafe impl Sync for Buffer {}

    static BUFFER: Buffer = Buffer(UnsafeCell::new([0; BYTES]));
    static USED: AtomicUsize = AtomicUsize::new(0);

    fn start() -> usize {
        BUFFER.0.get().addr()
    }

    /// The word before each block, holding its size: C's `realloc` and `malloc_usable_size`
    /// are given a block's address alone.
    const HEADER: usize = size_of::<usize>();

    /// A block for `layout` from the buffer, zero (`.bss`, never reused), its size in the word
    /// before it ([`size`]). Only the thread in `start` calls it (every other waits on its
    /// `Once`), so the bump is a load and a store.
    pub(super) fn alloc(layout: Layout) -> *mut u8 {
        let at = USED.load(Ordering::Relaxed);
        let align = layout.align().max(HEADER);
        let from = (start() + at + HEADER).next_multiple_of(align) - start();
        let Some(end) = from.checked_add(layout.size()).filter(|end| *end <= BYTES) else {
            super::say(b"blaze: the global heap's start allocated past its 1 MiB boot buffer (crates/blaze-engine/src/heaps.rs boot); blaze stops\n");
            std::process::abort()
        };
        USED.store(end, Ordering::Relaxed);
        let block = BUFFER.0.get().cast::<u8>().wrapping_add(from);
        // SAFETY: the word before the block is inside the buffer (`from >= HEADER`), aligned
        // for a `usize` (`from` is a multiple of `HEADER`), and no other block's.
        unsafe { block.sub(HEADER).cast::<usize>().write(layout.size()) };
        block
    }

    /// The size the boot block at `ptr` was made with.
    ///
    /// # Safety
    ///
    /// `ptr` is a block [`alloc`] returned.
    #[inline]
    pub(super) unsafe fn size(ptr: *const u8) -> usize {
        // SAFETY: the caller says `ptr` is a boot block, whose header `alloc` wrote.
        unsafe { ptr.sub(HEADER).cast::<usize>().read() }
    }

    /// Whether `ptr` is a boot block.
    #[inline(always)]
    pub(super) fn contains(ptr: *const u8) -> bool {
        ptr.addr().wrapping_sub(start()) < BYTES
    }
}

/// The size of `ptr` if it is a block made while the arena was being set up ([`boot`]): no
/// tag's, never freed, and unknown to mimalloc and to every other allocator, so C's `free`,
/// `realloc` and `malloc_usable_size` ask this before either.
#[inline]
pub(crate) fn boot_block(ptr: *const u8) -> Option<usize> {
    // SAFETY: inside the boot buffer every address C or Rust holds is a block `boot::alloc`
    // returned.
    boot::contains(ptr).then(|| unsafe { boot::size(ptr) })
}

/// An allocation the arena could not serve: its holder, size and alignment, the arena then,
/// and what refused it. Said on standard error as it happens ([`arena_full`]) and kept as the
/// last one ([`last_arena_full`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ArenaFull {
    pub holder: Holder,
    pub bytes: usize,
    pub align: usize,
    pub reserved: usize,
    pub registered: usize,
    pub committed: usize,
    /// The commit the gate or the kernel refused on the way: its bytes and the system's error
    /// number (-1 for the gate's refusal).
    pub refused: Option<(usize, i32)>,
}

impl fmt::Display for ArenaFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { holder, bytes, align, reserved, registered, committed, refused } = *self;
        write!(f, "blaze: the global heap's arena could not serve {bytes} B aligned to {align} ")?;
        match (holder.tag, holder.task.place()) {
            (Some(tag), Some(_)) => write!(f, "for a {tag:?} task's own heap")?,
            (Some(tag), None) => write!(f, "for {tag:?}'s heap")?,
            (None, _) => write!(f, "outside any task")?,
        }
        write!(
            f,
            " ({committed} B committed of {registered} B given to mimalloc, of {reserved} B \
             reserved): "
        )?;
        match refused {
            Some((bytes, -1)) => write!(f, "the memory gate refused committing {bytes} B more")?,
            Some((bytes, os)) => write!(f, "the kernel refused committing {bytes} B (os error {os})")?,
            None if bytes > MAX_OBJECT => write!(f, "a block past the arena's largest, {MAX_OBJECT} B")?,
            None => write!(f, "no run of free slices in the sub-arenas fits it")?,
        }
        write!(
            f,
            "; mimalloc takes no memory from the system past its arena (rule no-fallback, \
             crates/blaze-engine/src/heaps.rs)"
        )
    }
}

/// The last [`ArenaFull`], in words a reader takes apart: how many there were, then the holder
/// (tag code, task place), the size and the alignment.
static FULLS: AtomicU64 = AtomicU64::new(0);
static FULL_HOLDER: AtomicU64 = AtomicU64::new(0);
static FULL_BYTES: AtomicUsize = AtomicUsize::new(0);
static FULL_ALIGN: AtomicUsize = AtomicUsize::new(0);

/// mimalloc gave null for `into`: says the [`ArenaFull`] and keeps it, and gives the null on.
/// Allocates nothing.
#[cold]
#[inline(never)]
fn arena_full(into: Holder, bytes: usize, align: usize) -> *mut u8 {
    let refused = REFUSED.replace((0, 0));
    let (reserved, registered, committed) = MAIN.get().map_or((0, 0, 0), |arena| {
        (arena.usable, arena.registered.load(Ordering::Acquire), arena.region.held())
    });
    let full = ArenaFull {
        holder: into,
        bytes,
        align,
        reserved,
        registered,
        committed,
        refused: (refused.0 != 0).then_some(refused),
    };
    let tag = into.tag.map_or(0, |tag| tag.index() as u64 + 1);
    let place = into.task.place().map_or(0, |place| place as u64 + 1);
    FULL_HOLDER.store(tag | place << 8, Ordering::Relaxed);
    FULL_BYTES.store(bytes, Ordering::Relaxed);
    FULL_ALIGN.store(align, Ordering::Relaxed);
    FULLS.fetch_add(1, Ordering::Release);
    _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!("{full}\n"));
    ptr::null_mut()
}

/// How many allocations the arena could not serve, and the last one's holder's tag, size and
/// alignment.
#[cfg_attr(not(test), allow(dead_code, reason = "the memory line's to read: needed elsewhere"))]
#[cfg_attr(
    not(test),
    expect(blaze::unwired, reason = "unwired until the memory line reads it; only the tests call it")
)]
pub(crate) fn last_arena_full() -> (u64, Option<Tag>, usize, usize) {
    let count = FULLS.load(Ordering::Acquire);
    let tag = (FULL_HOLDER.load(Ordering::Relaxed) & 0xff) as usize;
    let tag = tag.checked_sub(1).and_then(|at| Tag::ALL.get(at).copied());
    (count, tag, FULL_BYTES.load(Ordering::Relaxed), FULL_ALIGN.load(Ordering::Relaxed))
}

/// mimalloc's page map, read for a live block's heap.
#[inline(always)]
fn heap_of(ptr: *const u8) -> *mut mi_heap_t {
    let slice = ptr.addr() >> SLICE_SHIFT;
    // SAFETY: mimalloc set the map before it made `ptr`, and a live block's slice has a sub-map
    // and a page in it (a page names itself at each of its slices, so an over-aligned block's
    // address inside it finds it too); the page stays while the block lives, and `HEAP_AT` is
    // its heap field (`check_page_map`). Reads only.
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

/// Has mimalloc give a page back to the system as it frees it to its arena, not `purge_delay`
/// (1 s) later. [`start`] sets it before the first block, since a purge is how the arena's
/// pages leave the gate's count; this is kept for `program.rs`'s call, which it makes redundant.
pub(crate) fn purge_at_once() {
    // SAFETY: sets one option word; settable from any thread at any time.
    unsafe { mi_option_set(PURGE_DELAY, 0) };
}

/// mimalloc, a heap per tag and per opted-in task ([`Holders`]).
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
    /// code that could allocate on it. A task heap's theap is never kept here: the heap is
    /// destroyed with its theaps as its task ends.
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
/// owner looks; the first forced collect also purges every arena's freed pages now. Allocates
/// nothing: mimalloc gives pages back to its arenas and purges them through [`commit`].
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
    // Its pages are back in the arenas and purged (unless another thread's purge held
    // mimalloc's purge guard, which the purge that waits on this redoes): a purge waiting for
    // every worker hears this one ([`blaze_mem::tag::heap_give_back_settled`]).
    blaze_mem::tag::heap_gave_back(asked);
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

/// Allocates from `theap` (of `heap`): the plain entry points within a word's alignment, the
/// heap's own for a zeroed block aligned past it, which no theap call makes.
///
/// # Safety
///
/// `theap` is this thread's theap of the live `heap`; a size and alignment of a valid layout,
/// the alignment at most [`MAX_ALIGN`].
#[inline(always)]
unsafe fn from_theap(
    theap: *mut mi_theap_t,
    heap: impl FnOnce() -> Option<*mut mi_heap_t>,
    size: usize,
    align: usize,
    zeroed: bool,
) -> *mut u8 {
    // SAFETY: as the caller says.
    unsafe {
        match (align <= WORD, zeroed) {
            (true, false) => mi_theap_malloc(theap, size).cast(),
            (true, true) => mi_theap_zalloc(theap, size).cast(),
            (false, false) => mi_theap_malloc_aligned(theap, size, align).cast(),
            (false, true) => match heap() {
                Some(heap) => mi_heap_zalloc_aligned(heap, size, align).cast(),
                None => no_heap(),
            },
        }
    }
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
        // layout within `MAX_ALIGN` (`alloc_into`).
        unsafe { from_theap(theap, || heap(tag), size, align, zeroed) }
    })
}

/// Allocates in the task heap at `place` of the pool, from this thread's theap of it.
#[inline(never)]
fn alloc_task(place: usize, size: usize, align: usize, zeroed: bool) -> *mut u8 {
    settle();
    let heap = task_heap(place);
    // SAFETY: an open task heap: its task is running here, so it is not destroyed meanwhile.
    let theap = unsafe { mi_heap_theap(heap) };
    if theap.is_null() {
        return no_heap();
    }
    // SAFETY: this thread's theap of the open heap; a valid layout within `MAX_ALIGN`.
    unsafe { from_theap(theap, || Some(heap), size, align, zeroed) }
}

/// The main heap's block, for an allocation outside any task.
fn alloc_main(size: usize, align: usize, zeroed: bool) -> *mut u8 {
    // SAFETY: a size and alignment of a valid layout, within `MAX_ALIGN`.
    unsafe {
        match (align <= WORD, zeroed) {
            (true, false) => mi_malloc(size).cast(),
            (true, true) => mi_zalloc(size).cast(),
            (false, false) => mi_malloc_aligned(size, align).cast(),
            (false, true) => mi_zalloc_aligned(size, align).cast(),
        }
    }
}

/// A block in `into` (a tag's heap, a task heap, or the main heap outside any task) of `size`
/// bytes aligned to `align`, at most [`MAX_ALIGN`]; null as mimalloc gives it.
fn made_in(into: Holder, size: usize, align: usize, zeroed: bool) -> *mut u8 {
    match (into.task.place(), into.tag) {
        (Some(place), _) => alloc_task(place, size, align, zeroed),
        (None, Some(tag)) => alloc_in(tag, size, align, zeroed),
        (None, None) => alloc_main(size, align, zeroed),
    }
}

/// A block for `layout` in `into`; null only as the typed [`ArenaFull`]. The arena is set up
/// first, and while it is, its own thread's blocks are [`boot`]'s.
#[inline(always)]
fn alloc_into(into: Holder, layout: Layout, zeroed: bool) -> *mut u8 {
    if !ready() {
        return boot::alloc(layout);
    }
    let (size, align) = (layout.size(), layout.align());
    let block = match align > MAX_ALIGN {
        true => over_aligned(size, align, |outer| made_in(into, outer, WORD, zeroed)),
        false => made_in(into, size, align, zeroed),
    };
    match block.is_null() {
        true => arena_full(into, size, align),
        false => block,
    }
}

/// A block aligned past [`MAX_ALIGN`], where mimalloc would map one from the system: `size +
/// align` bytes made by `made`, the returned address the first aligned one at least a word
/// in, and the made block's address in the word before it ([`unaligned`]). At most `align`
/// bytes are spent on it, as a system mapping of its own would round to the alignment too.
#[cold]
fn over_aligned(size: usize, align: usize, made: impl FnOnce(usize) -> *mut u8) -> *mut u8 {
    let Some(outer) = size.checked_add(align) else { return ptr::null_mut() };
    let base = made(outer);
    if base.is_null() {
        return base;
    }
    // `base` is aligned to 16 (every block of mimalloc's of 16 bytes or more), so the first
    // aligned address a word past it is at most `align` past it, and `size` bytes fit.
    let at = (base.addr() + WORD).next_multiple_of(align) - base.addr();
    // SAFETY: `at + size <= outer`, inside the block; the word before it too (`at >= WORD`).
    unsafe {
        let block = base.add(at);
        block.cast::<*mut u8>().sub(1).write(base);
        block
    }
}

/// The block mimalloc made for `ptr`, made for `layout`.
///
/// # Safety
///
/// `ptr` is a live block of this allocator made for `layout`.
#[inline(always)]
unsafe fn unaligned(ptr: *mut u8, layout: Layout) -> *mut u8 {
    match layout.align() > MAX_ALIGN {
        // SAFETY: `over_aligned` wrote the made block's address in the word before it.
        true => unsafe { ptr.cast::<*mut u8>().sub(1).read() },
        false => ptr,
    }
}

/// From a heap to its holder, for [`holder`]: open addressing over [`SLOTS`] slots, a heap's
/// first slot by its address. A slot holds the heap's address in its low 48 bits and a code in
/// its top 16 (a tag's index plus one, or [`POOL_CODE`] with a task heap's place), so one
/// compare-and-swap publishes both; 0 is an empty slot, [`TOMB`] one a destroyed task heap left.
const SLOTS: usize = 4096;
static FILED: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// The bits of a slot that are the heap's address.
const ADDRESS: u64 = (1 << 48) - 1;
/// A task heap's code: this bit and its place.
const POOL_CODE: u64 = 1 << 15;
/// A slot whose heap was destroyed: a lookup goes past it, a filing may take it.
const TOMB: u64 = 1;

const _: () = assert!((Tag::COUNT + MAX_TASK_HEAPS) * 2 <= SLOTS, "the table stays at most half live");
const _: () = assert!(MAX_TASK_HEAPS as u64 <= POOL_CODE, "a place fits beside the pool's bit");

#[inline(always)]
fn first_slot(heap: *mut mi_heap_t) -> usize {
    // Fibonacci hashing of the address, past the bits its alignment fixes.
    ((heap.addr() >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        >> (usize::BITS - SLOTS.trailing_zeros())) as usize
}

/// Says `heap` is `code`'s in the table [`holder`] reads; `false` where its address does not
/// fit a slot, or the table is full, so no block of it could be named.
fn file(heap: *mut mi_heap_t, code: u64) -> bool {
    if heap.addr() as u64 & !ADDRESS != 0 {
        return false;
    }
    let entry = heap.addr() as u64 | code << 48;
    let mut at = first_slot(heap);
    for _ in 0..SLOTS {
        let seen = FILED[at].load(Ordering::Relaxed);
        if (seen == 0 || seen == TOMB)
            && FILED[at].compare_exchange(seen, entry, Ordering::Release, Ordering::Relaxed).is_ok()
        {
            return true;
        }
        at = (at + 1) % SLOTS;
    }
    false
}

/// Takes `heap` out of the table, leaving a [`TOMB`].
fn unfile(heap: *mut mi_heap_t) {
    let mut at = first_slot(heap);
    for _ in 0..SLOTS {
        let seen = FILED[at].load(Ordering::Relaxed);
        if seen & ADDRESS == heap.addr() as u64 {
            FILED[at].store(TOMB, Ordering::Release);
            return;
        }
        if seen == 0 {
            break;
        }
        at = (at + 1) % SLOTS;
    }
    say(b"blaze: a task heap being destroyed was not in the global heap's table (crates/blaze-engine/src/heaps.rs unfile); its blocks were counted to no tag\n");
    std::process::abort()
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
    if !ready() {
        return None;
    }
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
            if !file(made, tag.index() as u64 + 1) {
                unnamed(made);
            }
            blaze_mem::tag::heap_by_tag();
            blaze_mem::tag::heap_committed_by(committed_now);
            blaze_mem::tag::heap_gives_back_by(settle);
            blaze_mem::tag::heap_pages_by(pages);
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

/// Where `ptr`, a live block of mimalloc's, is: its tag's heap, a task heap, or the main heap
/// ([`Holder::OUTSIDE`]). Most heaps are found at their first slot: a multiply and a load past
/// [`heap_of`].
#[inline(always)]
fn holder(ptr: *mut u8) -> Holder {
    if boot::contains(ptr) {
        return Holder::OUTSIDE;
    }
    let heap = heap_of(ptr);
    if heap.is_null() {
        return Holder::OUTSIDE;
    }
    let mut at = first_slot(heap);
    for _ in 0..SLOTS {
        // Relaxed: the entry is one word, address and code together, and nothing else is read
        // through it but a task heap's tag, written before the heap was filed.
        let entry = FILED[at].load(Ordering::Relaxed);
        if entry == 0 {
            return Holder::OUTSIDE;
        }
        if entry & ADDRESS == heap.addr() as u64 {
            let code = entry >> 48;
            if code & POOL_CODE == 0 {
                return Tag::ALL.get(code as usize - 1).map_or(Holder::OUTSIDE, |tag| Holder::of(*tag));
            }
            let place = (code & !POOL_CODE) as usize;
            let tag = Tag::ALL[usize::from(POOL[place].tag.load(Ordering::Relaxed))];
            return Holder::in_task(TaskHeap::new(place, tag));
        }
        at = (at + 1) % SLOTS;
    }
    Holder::OUTSIDE
}

/// One place of the task-heap pool: the heap open there (null while free) and its task's tag.
struct Place {
    heap: AtomicPtr<mi_heap_t>,
    tag: AtomicU8,
}

static POOL: [Place; MAX_TASK_HEAPS] =
    [const { Place { heap: AtomicPtr::new(ptr::null_mut()), tag: AtomicU8::new(0) } };
        MAX_TASK_HEAPS];

/// Which places are held, a bit each.
static TAKEN: [AtomicU64; MAX_TASK_HEAPS / 64] = [const { AtomicU64::new(0) }; MAX_TASK_HEAPS / 64];

const _: () = assert!(MAX_TASK_HEAPS % 64 == 0 && Tag::COUNT <= 127);

/// A free place, now held; `None` when every one is. Each word is tried until it is full or a
/// claim on it was lost 64 times (other takers moving it, every loss one of theirs won).
fn take_place() -> Option<usize> {
    for (at, word) in TAKEN.iter().enumerate() {
        for _ in 0..64 {
            let held = word.load(Ordering::Relaxed);
            if held == !0 {
                break;
            }
            let bit = !held & held.wrapping_add(1);
            if word.fetch_or(bit, Ordering::AcqRel) & bit == 0 {
                return Some(at * 64 + bit.trailing_zeros() as usize);
            }
        }
    }
    None
}

/// Opens a heap for a task of `tag` ([`TaskHeaps::open`]): a fresh mimalloc heap at a free
/// place of the pool. The floor: one `mi_heap_new` (its record from the main heap), and its
/// page bitmaps in each sub-arena it touches as it allocates (about 600 KiB each, zero and
/// untouched but for the bits it sets).
fn open_task_heap(tag: Tag) -> Result<TaskHeap, TaskHeapRefused> {
    // The pool is said to blaze-graph only once the arena is ready, so it is.
    let place = take_place().ok_or(TaskHeapRefused { tag, open: MAX_TASK_HEAPS })?;
    // SAFETY: makes a heap; its record is the main heap's, not the global allocator's.
    let heap = unsafe { mi_heap_new() };
    if heap.is_null() {
        _ = std::io::Write::write_fmt(
            &mut std::io::stderr(),
            format_args!(
                "blaze: mimalloc made no heap for a {tag:?} task (mi_heap_new returned null; the \
                 global heap's arena is out of room); blaze stops (crates/blaze-engine/src/heaps.rs)\n"
            ),
        );
        std::process::abort();
    }
    POOL[place].tag.store(tag.index() as u8, Ordering::Relaxed);
    if !file(heap, POOL_CODE | place as u64) {
        unnamed(heap);
    }
    POOL[place].heap.store(heap, Ordering::Release);
    Ok(TaskHeap::new(place, tag))
}

/// Destroys the task heap `task` with every block in it, and frees its place
/// ([`TaskHeaps::close`]): its pages go back to the arena and, at `purge_delay` 0, out of the
/// gate's count at once. In a debug or test build they are quarantined first
/// ([`quarantine::mark`]).
fn close_task_heap(task: TaskHeap) {
    let (Some(place), Some(tag)) = (task.place(), task.tag()) else {
        say(b"blaze: closing no task heap (crates/blaze-engine/src/heaps.rs close_task_heap)\n");
        std::process::abort()
    };
    let heap = POOL[place].heap.swap(ptr::null_mut(), Ordering::AcqRel);
    if heap.is_null() {
        closed(place);
    }
    #[cfg(debug_assertions)]
    quarantine::mark(heap, tag);
    #[cfg(not(debug_assertions))]
    let _ = tag;
    unfile(heap);
    // SAFETY: a heap of the pool's, whose task ended: nothing allocates in it again (its place
    // is free only after this), and its blocks are reached by nothing (the opt-in's contract).
    unsafe { mi_heap_destroy(heap) };
    TAKEN[place / 64].fetch_and(!(1 << (place % 64)), Ordering::AcqRel);
}

/// The open task heap at `place`.
#[inline(always)]
fn task_heap(place: usize) -> *mut mi_heap_t {
    let heap = POOL[place].heap.load(Ordering::Acquire);
    if heap.is_null() {
        closed(place);
    }
    heap
}

/// A task heap used after its task ended, or closed twice.
#[cold]
#[inline(never)]
fn closed(place: usize) -> ! {
    let tag = Tag::ALL.get(usize::from(POOL[place].tag.load(Ordering::Relaxed)));
    _ = std::io::Write::write_fmt(
        &mut std::io::stderr(),
        format_args!(
            "blaze: the heap of a {tag:?} task (place {place} of the task-heap pool) was used after \
             its task ended: a value of the task's, kept past it, allocated or was freed into a \
             heap that died with it (blaze_graph::arena::heap_dies_with_task); blaze stops\n"
        ),
    );
    std::process::abort()
}

/// What a fault at an address read: a block of a task heap destroyed as its task ended, kept
/// past it ([`kept_past_task`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "the crash handler's to call: needed elsewhere"))]
#[cfg_attr(
    not(test),
    expect(blaze::unwired, reason = "unwired until the crash handler names the task's tag at a fault")
)]
pub(crate) struct KeptPastTask {
    pub tag: Tag,
    pub addr: usize,
}

impl fmt::Display for KeptPastTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a value kept past the end of its {:?} task, whose heap died with it \
             (blaze_graph::arena::heap_dies_with_task), was reached at {:#x}",
            self.tag, self.addr
        )
    }
}

/// Whether `addr`, where a fault happened, is in a quarantined page of a destroyed task heap,
/// and whose: what a crash handler says. Only a debug or test build quarantines; a release build
/// answers `None`.
#[cfg_attr(not(test), allow(dead_code, reason = "the crash handler's to call: needed elsewhere"))]
#[cfg_attr(
    not(test),
    expect(blaze::unwired, reason = "unwired until the crash handler names the task's tag at a fault")
)]
pub(crate) fn kept_past_task(addr: usize) -> Option<KeptPastTask> {
    #[cfg(debug_assertions)]
    {
        quarantine::tag_at(addr).map(|tag| KeptPastTask { tag, addr })
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = addr;
        None
    }
}

/// A debug or test build's quarantine of destroyed task heaps' pages: a byte a slice of the
/// main arena (8 MiB of `.bss`, touched only where heaps died), the tag's index plus one while
/// the slice is a dying heap's and [`GUARDED`] once it is inaccessible.
#[cfg(debug_assertions)]
mod quarantine {
    use std::{
        ffi::c_void,
        ptr,
        sync::atomic::{AtomicU8, Ordering},
    };

    use blaze_mem::Tag;
    use libmimalloc_sys::mi_heap_t;

    use super::{Arena, MAIN, RESERVE, SLICE, SLICE_SHIFT, mi_heap_visit_blocks, mprotect, say};

    static DIED: [AtomicU8; RESERVE / SLICE] = [const { AtomicU8::new(0) }; RESERVE / SLICE];
    const GUARDED: u8 = 0x80;
    const PROT_NONE: i32 = 0;
    const PROT_READ_WRITE: i32 = 1 | 2;

    /// Marks the slices of `heap`'s pages as dying with a task of `tag`: as the heap is
    /// destroyed and they are purged, [`guard`] makes them inaccessible. A slice whose purge
    /// another thread's purge guard put off stays marked until it is next purged.
    pub(super) fn mark(heap: *mut mi_heap_t, tag: Tag) {
        let Some(arena) = MAIN.get() else { return };
        let mut into = (ptr::from_ref(arena), tag.index() as u8 + 1);
        // SAFETY: a live heap of the pool's; the visitor reads its arguments.
        unsafe { mi_heap_visit_blocks(heap, false, mark_page, ptr::from_mut(&mut into).cast()) };
    }

    unsafe extern "C" fn mark_page(
        _: *const mi_heap_t,
        area: *const super::Area,
        _: *mut c_void,
        _: usize,
        arg: *mut c_void,
    ) -> bool {
        // SAFETY: `mark`'s pair, its alone for the walk, and the page's record on mimalloc's
        // stack for this call.
        let ((arena, code), area) = unsafe { (*arg.cast::<(*const Arena, u8)>(), &*area) };
        // SAFETY: the main arena, which lives for the process.
        let arena = unsafe { &*arena };
        let from = area.blocks.addr() & !(SLICE - 1);
        let to = (area.blocks.addr() + area.reserved).next_multiple_of(SLICE);
        if arena.contains(from) {
            for slice in (from - arena.start) >> SLICE_SHIFT..(to - arena.start) >> SLICE_SHIFT {
                DIED[slice].store(code, Ordering::Relaxed);
            }
        }
        true
    }

    /// Slices `[from, to)`, just purged: those of a dying heap become inaccessible.
    pub(super) fn guard(arena: &Arena, from: usize, to: usize) {
        protect(arena, from, to, |code| code != 0 && code & GUARDED == 0, PROT_NONE, |code| {
            code | GUARDED
        });
    }

    /// Slices `[first, end)`, about to be committed for a new use: accessible again, and no
    /// heap's.
    pub(super) fn release(arena: &Arena, first: usize, end: usize) {
        protect(arena, first, end, |code| code & GUARDED != 0, PROT_READ_WRITE, |_| 0);
        for slice in first..end {
            DIED[slice].store(0, Ordering::Relaxed);
        }
    }

    /// `mprotect`s each run of slices of `[from, to)` whose byte `pick`s to `prot`, rewriting
    /// their bytes with `then`.
    fn protect(
        arena: &Arena,
        from: usize,
        to: usize,
        pick: impl Fn(u8) -> bool,
        prot: i32,
        then: impl Fn(u8) -> u8,
    ) {
        let mut slice = from;
        while slice < to {
            if !pick(DIED[slice].load(Ordering::Relaxed)) {
                slice += 1;
                continue;
            }
            let run = slice;
            while slice < to && pick(DIED[slice].load(Ordering::Relaxed)) {
                let code = DIED[slice].load(Ordering::Relaxed);
                DIED[slice].store(then(code), Ordering::Relaxed);
                slice += 1;
            }
            let at = arena.region.base().as_ptr().wrapping_add(arena.offset(run));
            // SAFETY: whole slices of the arena's live region, free (being purged, or about to
            // be committed), which nothing but a value kept past its task reaches.
            if unsafe { mprotect(at.cast(), (slice - run) * SLICE, prot) } != 0 {
                say(b"blaze: mprotect of the global heap's quarantined pages failed (crates/blaze-engine/src/heaps.rs quarantine)\n");
                std::process::abort();
            }
        }
    }

    /// The tag of the task whose heap died with the quarantined slice at `addr`.
    #[cfg_attr(not(test), allow(dead_code, reason = "kept_past_task's, a crash handler's"))]
    #[cfg_attr(
        not(test),
        expect(blaze::unwired, reason = "unwired until the crash handler calls `kept_past_task`")
    )]
    pub(super) fn tag_at(addr: usize) -> Option<Tag> {
        let arena = MAIN.get()?;
        if !arena.contains(addr) {
            return None;
        }
        let code = DIED[(addr - arena.start) >> SLICE_SHIFT].load(Ordering::Relaxed);
        if code & GUARDED == 0 {
            return None;
        }
        Tag::ALL.get(usize::from(code & !GUARDED).checked_sub(1)?).copied()
    }
}

/// What mimalloc has committed now, over every heap: what its arena holds through the gate
/// ([`Region::held`]): its live blocks, the free pages it keeps (a few per thread and size
/// class, `page_full_retain`), and its own metadata, all of it counted as it is committed. The
/// purge at `purge_delay` 0 takes a freed page out of it at once; a page another thread freed
/// into, until its owner collects ([`blaze_mem::tag::heap_give_back_settled`]). Allocates
/// nothing.
pub(crate) fn committed_now() -> u64 {
    MAIN.get().map_or(0, |arena| arena.region.held() as u64)
}

/// mimalloc 3's `mi_heap_area_t` (include/mimalloc.h): one page of a heap, as
/// `mi_heap_visit_blocks` hands it to its visitor. libmimalloc-sys names mimalloc 2's, two
/// fields shorter.
#[repr(C)]
struct Area {
    blocks: *mut c_void,
    /// The bytes the page reserves for blocks.
    reserved: usize,
    /// The page's capacity times its block size.
    committed: usize,
    /// Live blocks.
    used: usize,
    block_size: usize,
    full_block_size: usize,
    reserved1: *mut c_void,
}

unsafe extern "C" {
    /// Calls `visitor` once a page of `heap` (`visit_blocks` false), whichever thread's theap
    /// has it, by a walk of the heap's page bitmaps in every arena; null is the main heap.
    /// mimalloc takes no lock for it: it reads each page's header as the page's owner writes
    /// it, and a page freed meanwhile reads as it was or, once purged, as zeroes.
    fn mi_heap_visit_blocks(
        heap: *mut mi_heap_t,
        visit_blocks: bool,
        visitor: unsafe extern "C" fn(
            *const mi_heap_t,
            *const Area,
            *mut c_void,
            usize,
            *mut c_void,
        ) -> bool,
        arg: *mut c_void,
    ) -> bool;
}

/// Adds one page to the [`blaze_mem::tag::HeapPages`] `arg` points at.
unsafe extern "C" fn count_page(
    _: *const mi_heap_t,
    area: *const Area,
    _: *mut c_void,
    _: usize,
    arg: *mut c_void,
) -> bool {
    // SAFETY: `arg` is the `HeapPages` `pages` passed, its alone for the walk, and `area` a
    // record on mimalloc's stack for this call.
    let (into, area) = unsafe { (&mut *arg.cast::<blaze_mem::tag::HeapPages>(), &*area) };
    let committed = area.committed as u64;
    // A header read mid-write may say more blocks than the page has: held to the page.
    let used = (area.used as u64).saturating_mul(area.full_block_size as u64).min(committed);
    into.pages += 1;
    into.committed += committed;
    into.used += used;
    if used == 0 {
        into.empty += committed;
    } else if used < committed / 4 {
        into.sparse += committed;
    }
    true
}

/// What each tag's heap holds in its pages, then the main heap's ([`blaze_mem::tag::heap_pages`]):
/// a walk of mimalloc's page bitmaps, a header a page, with other threads allocating. It
/// allocates nothing. A task heap's pages are in no line here: they are gone as its task ends.
fn pages(into: &mut [blaze_mem::tag::HeapPages; Tag::COUNT + 1]) {
    for (tag, into) in Tag::ALL.into_iter().zip(into.iter_mut()) {
        let heap = HEAPS[tag.index()].load(Ordering::Acquire);
        if !heap.is_null() {
            // SAFETY: a heap of ours, never deleted; the visitor reads its two arguments.
            unsafe { mi_heap_visit_blocks(heap, false, count_page, ptr::from_mut(into).cast()) };
        }
    }
    // SAFETY: null names the main heap; as above.
    unsafe {
        mi_heap_visit_blocks(
            ptr::null_mut(),
            false,
            count_page,
            ptr::from_mut(&mut into[Tag::COUNT]).cast(),
        );
    }
}

/// mimalloc could make no heap for a tag: said before the allocation fails, since the failure
/// alone would read as running out of memory.
#[cold]
fn no_heap() -> *mut u8 {
    say(b"blaze: mimalloc made no heap for a blaze-mem tag (mi_heap_new returned null), so this allocation fails\n");
    ptr::null_mut()
}

/// A heap at an address past what a slot holds, or a full table: its blocks would count to no
/// tag, so blaze stops, saying so, rather than name the global heap wrongly.
#[cold]
fn unnamed(_: *mut mi_heap_t) -> ! {
    say(b"blaze: a mimalloc heap lies above 2^48, past what blaze's heap table holds, or the table is full (crates/blaze-engine/src/heaps.rs)\n");
    std::process::abort()
}

/// Writes `said` to standard error without allocating.
fn say(said: &[u8]) {
    _ = std::io::Write::write_all(&mut std::io::stderr(), said);
}

/// A C caller asked a block aligned past [`MAX_ALIGN`] (`posix_memalign`, `aligned_alloc`,
/// `memalign`, a zone's `memalign`): mimalloc would map it from the system, which the arena
/// disallows, and C frees by address alone, so [`over_aligned`]'s header cannot serve it. Said,
/// and refused with null.
#[cold]
pub(crate) fn c_alignment_refused(size: usize, align: usize) -> *mut u8 {
    _ = std::io::Write::write_fmt(
        &mut std::io::stderr(),
        format_args!(
            "blaze: C asked for {size} B aligned to {align}, past the {MAX_ALIGN} the global heap's \
             arena aligns a block a C caller frees by address (crates/blaze-engine/src/heaps.rs); \
             refused\n"
        ),
    );
    ptr::null_mut()
}

// SAFETY: each call is mimalloc's for the same layout (its `_aligned` functions take any
// power-of-two alignment up to `MAX_ALIGN`, and `over_aligned` serves the rest), in the main
// heap; `mi_free` frees a block of any heap from any thread.
unsafe impl GlobalAlloc for Heaps {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        alloc_into(Holder::OUTSIDE, layout, false)
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        alloc_into(Holder::OUTSIDE, layout, true)
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if boot::contains(ptr) {
            return;
        }
        // SAFETY: the caller says `ptr` is a live block of this allocator made for `layout`.
        unsafe { mi_free(unaligned(ptr, layout).cast()) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `realloc_for` outside any task.
        unsafe { self.realloc_for(Holder::OUTSIDE, ptr, layout, new_size) }
    }
}

impl Holders for Heaps {
    const BY_TAG: bool = true;

    #[inline]
    unsafe fn alloc_for(&self, into: Holder, layout: Layout, zeroed: bool) -> *mut u8 {
        alloc_into(into, layout, zeroed)
    }

    #[inline]
    unsafe fn holder(&self, ptr: *mut u8) -> Holder {
        holder(ptr)
    }

    #[inline]
    unsafe fn realloc_for(
        &self,
        into: Holder,
        ptr: *mut u8,
        layout: Layout,
        new_size: usize,
    ) -> *mut u8 {
        if layout.align() > MAX_ALIGN || boot::contains(ptr) || !ready() {
            // SAFETY: the caller upholds `realloc`'s contract.
            return unsafe { moved(into, ptr, layout, new_size) };
        }
        // SAFETY: the caller upholds `realloc`'s contract.
        unsafe { realloc_into(into, ptr, new_size, layout.align()) }
    }
}

/// mimalloc's realloc of `ptr` to `new_size` at `align` (at most [`MAX_ALIGN`]): a block that
/// still fits stays where it is, on its heap; one that moves goes to `into`. Null only as the
/// typed [`ArenaFull`].
///
/// # Safety
///
/// `realloc`'s contract for `ptr`; `into`'s task heap, if any, is open.
unsafe fn realloc_into(into: Holder, ptr: *mut u8, new_size: usize, align: usize) -> *mut u8 {
    // SAFETY: as the caller says; each heap is live.
    let block: *mut u8 = unsafe {
        match (into.task.place(), into.tag) {
            (Some(place), _) => {
                mi_heap_realloc_aligned(task_heap(place), ptr.cast(), new_size, align).cast()
            }
            (None, Some(tag)) => {
                let Some(heap) = heap(tag) else { return no_heap() };
                mi_heap_realloc_aligned(heap, ptr.cast(), new_size, align).cast()
            }
            (None, None) => mi_realloc_aligned(ptr.cast(), new_size, align).cast(),
        }
    };
    match block.is_null() {
        true => arena_full(into, new_size, align),
        false => block,
    }
}

/// A realloc of a block aligned past [`MAX_ALIGN`]: a new block, the bytes copied, the old one
/// freed (a [`boot`] block is left, never reused): also a realloc of a boot block, or one
/// made while the arena is set up.
///
/// # Safety
///
/// `realloc`'s contract for `ptr`, made for `layout`.
#[cold]
unsafe fn moved(into: Holder, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let Ok(grown) = Layout::from_size_align(new_size, layout.align()) else {
        return ptr::null_mut();
    };
    let new = alloc_into(into, grown, false);
    if !new.is_null() {
        // SAFETY: both blocks live, apart, each at least the bytes copied.
        unsafe { ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size)) };
        if !boot::contains(ptr) {
            // SAFETY: the old block, mimalloc's, freed once.
            unsafe { mi_free(unaligned(ptr, layout).cast()) };
        }
    }
    new
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
        // A block C asked for while the arena was being set up is a boot block, which
        // mimalloc's page map has no page for.
        if let Some(bytes) = boot_block(ptr) {
            return bytes;
        }
        // SAFETY: the caller says `ptr` is a live block of mimalloc's (C's, never over-aligned).
        unsafe { mi_usable_size(ptr.cast()) }
    }

    #[inline]
    unsafe fn free(&self, ptr: *mut u8) {
        // SAFETY: the caller says `ptr` is a live block of mimalloc's; `mi_free` frees a block
        // of any heap from any thread.
        unsafe { mi_free(ptr.cast()) }
    }

    #[inline]
    unsafe fn realloc_in(&self, into: Holder, ptr: *mut u8, new_size: usize) -> *mut u8 {
        // C's blocks are mimalloc's (`owns`), so the arena is ready: never a boot block.
        // SAFETY: the caller upholds `realloc`'s contract; at a word's alignment mimalloc's
        // `_aligned` reallocs are its plain ones.
        unsafe { realloc_into(into, ptr, new_size, WORD) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 16).expect("a valid layout")
    }

    /// Held by each test here: those that measure what mimalloc commits, or a page's quarantine,
    /// would count another's allocations as their own, or see their pages reused.
    static MEASURING: blaze_core::lock::Lock<()> =
        blaze_core::lock::Lock::new("engine.heaps.test.measuring", ());

    /// The global allocator's counting over mimalloc, as `lib.rs` installs it outside tests.
    static COUNTED: blaze_graph::arena::Counting<Heaps> = blaze_graph::arena::Counting(Heaps);

    /// A block made for a tag is found on that tag's heap from any thread, goes to the heap of
    /// the tag a realloc that moves it is for, and one made outside any task is no tag's.
    #[test]
    fn a_block_is_found_on_the_heap_of_the_tag_it_was_made_for() {
        let _measuring = MEASURING.lock();
        let heaps = Heaps;
        // SAFETY: valid layouts; each block is freed once, with its layout.
        unsafe {
            let infer = heaps.alloc_for(Holder::of(Tag::Infer), layout(48), false);
            let query = heaps.alloc_for(Holder::of(Tag::Query), layout(4 << 20), true);
            let plain = heaps.alloc(layout(48));
            assert!(!infer.is_null() && !query.is_null() && !plain.is_null());
            assert_eq!(heaps.holder(infer), Holder::of(Tag::Infer));
            assert_eq!(heaps.holder(query), Holder::of(Tag::Query));
            assert_eq!(heaps.holder(plain), Holder::OUTSIDE);
            assert_eq!(*query.add(12345), 0, "zeroed");
            // Moves: 48 bytes to 64 KiB is past any in-place growth.
            let moved = heaps.realloc_for(Holder::of(Tag::Mir), infer, layout(48), 64 << 10);
            assert_eq!(heaps.holder(moved), Holder::of(Tag::Mir));
            let sent = Sent(moved, query);
            std::thread::spawn(move || {
                let (moved, query) = sent.parts();
                assert_eq!(Heaps.holder(moved), Holder::of(Tag::Mir), "another thread finds it too");
                Heaps.dealloc(moved, layout(64 << 10));
                Heaps.dealloc(query, layout(4 << 20));
            })
            .join()
            .expect("the other thread frees both");
            heaps.dealloc(plain, layout(48));
        }
    }

    /// A free finds the heap of a block of any size and alignment from the page map as
    /// `mi_heap_of` does: blocks over-aligned past a page, past mimalloc's own alignment (made
    /// here, `over_aligned`), and huge ones sit at offsets in their pages that a small block's
    /// do not.
    #[test]
    fn the_page_map_names_each_blocks_heap_as_mimalloc_does() {
        let _measuring = MEASURING.lock();
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
                let block = heaps.alloc_for(Holder::of(Tag::Parse), layout, size % 2 == 0);
                assert!(
                    !block.is_null() && block.addr() % align == 0,
                    "{size} B aligned to {align}"
                );
                assert_eq!(heap_of(block), mi_heap_of(block.cast()), "{size} B aligned to {align}");
                assert_eq!(heaps.holder(block), Holder::of(Tag::Parse), "{size} B aligned to {align}");
                heaps.dealloc(block, layout);
            }
        }
    }

    /// mimalloc takes nothing from the system past the arena: blocks from a word to 256 MiB,
    /// in a tag's heap and the main one, all lie in what the arena gave it. The cause, by
    /// removing it: with `disallow_os_alloc` at 0 a block past the sub-arena's free room would
    /// be mapped apart from it.
    #[test]
    fn every_block_is_in_the_arena() {
        let _measuring = MEASURING.lock();
        for size in [8, 4 << 10, 1 << 20, 64 << 20, 256 << 20] {
            // SAFETY: valid layouts; each block freed once, with it.
            unsafe {
                for block in [Heaps.alloc_for(Holder::of(Tag::Parse), layout(size), false), Heaps.alloc(layout(size))] {
                    assert!(!block.is_null(), "{size} B");
                    let arena = MAIN.get().expect("the arena is set up by the first allocation");
                    assert!(arena.contains(block.addr()), "a {size} B block at {block:p} is outside the arena");
                    Heaps.dealloc(block, layout(size));
                }
            }
        }
    }

    /// The options [`start`] sets are the ones it means: mimalloc prints each option by name.
    #[test]
    fn the_options_are_the_ones_this_names() {
        unsafe extern "C" fn into(line: *const c_char, out: *mut c_void) {
            // SAFETY: `out` is the test's `String`, and `line` a C string of mimalloc's.
            unsafe {
                (*out.cast::<String>()).push_str(&std::ffi::CStr::from_ptr(line).to_string_lossy());
            }
        }
        ready();
        let mut printed = String::new();
        // SAFETY: `into` writes only to `printed`, which outlives the call.
        unsafe { mi_options_print_out(into, ptr::from_mut(&mut printed).cast()) };
        for (name, value) in [
            ("purge_delay", 0),
            ("disallow_os_alloc", 1),
            ("page_commit_on_demand", 0),
            ("arena_max_object_size", (MAX_OBJECT >> 10) as i64),
        ] {
            let line = format!("option '{name}': {value}");
            assert!(printed.contains(&line), "no `{line}` in mimalloc's options:\n{printed}");
        }
        // And by number: what each name's value is, read through the number this file uses.
        for (option, value) in [
            (PURGE_DELAY, 0),
            (DISALLOW_OS_ALLOC, 1),
            (PAGE_COMMIT_ON_DEMAND, 0),
            (ARENA_MAX_OBJECT_SIZE, (MAX_OBJECT >> 10) as c_long),
        ] {
            // SAFETY: reads an option word.
            assert_eq!(unsafe { libmimalloc_sys::mi_option_get(option) }, value, "option {option}");
        }
    }

    /// An allocation the arena cannot serve is the typed [`ArenaFull`], naming its tag, never a
    /// block the system mapped: a block past the largest the arena holds, in a tag's heap; and
    /// a test's own exclusive arena of 64 MiB filled with 1 MiB blocks gives null within its
    /// size, every block inside it.
    #[test]
    fn a_full_arena_is_the_typed_error() {
        let _measuring = MEASURING.lock();
        let (before, ..) = last_arena_full();
        let past = MAX_OBJECT + (1 << 30);
        // SAFETY: a valid layout; null, so nothing to free.
        let block = unsafe { Heaps.alloc_for(Holder::of(Tag::Fuzz), layout(past), false) };
        assert!(block.is_null(), "a {past} B block was made");
        let (after, tag, bytes, _) = last_arena_full();
        assert_eq!((after, tag, bytes), (before + 1, Some(Tag::Fuzz), past));

        const SIZE: usize = 64 << 20;
        let region = Region::reserve_sparse(SIZE + SLICE, Tag::Fuzz).expect("the test's arena");
        let bits: &'static [AtomicU64] =
            Vec::leak((0..SIZE / SLICE / 64 + 1).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(region, bits, false, true)));
        assert!(arena.register(0), "mimalloc took the test's arena");
        // SAFETY: the test's exclusive arena's id.
        let heap = unsafe { mi_heap_new_in_arena(arena.id.load(Ordering::Acquire)) };
        assert!(!heap.is_null());
        let mut made = 0;
        let full = loop {
            // SAFETY: a heap of the test's; its blocks are destroyed with it below.
            let block: *mut u8 = unsafe { mi_heap_malloc_aligned(heap, 1 << 20, 16) }.cast();
            if block.is_null() {
                break true;
            }
            assert!(arena.contains(block.addr()), "block {made} outside the test's arena");
            made += 1;
            if made > SIZE >> 20 {
                break false;
            }
        };
        assert!(full, "the 64 MiB arena held {made} blocks of 1 MiB");
        assert!(arena.region.held() <= SIZE, "{} B committed in a {SIZE} B arena", arena.region.held());
        // SAFETY: the test's heap, used by nothing after.
        unsafe { mi_heap_destroy(heap) };
    }

    /// The invariant: an opted-in task's heap dies with it. 64 MiB of blocks a task of
    /// [`Tag::Api`] makes in its own heap and leaves unfreed leave its
    /// `Counters::heap` where it was once the task ends, and the arena's committed bytes within
    /// 8 MiB of where they began. The control, a leaked heap: one opened and filled the same way
    /// but never closed keeps its bytes committed (and counted to its tag), so each check above
    /// would fail on it.
    #[test]
    fn a_task_heap_dies_with_its_task() {
        const BLOCK: usize = 1 << 20;
        const BLOCKS: usize = 64;
        const SLACK: u64 = 8 << 20;
        let _measuring = MEASURING.lock();
        ready();
        let heap_of_api = || {
            blaze_graph::arena::fold_here();
            blaze_mem::tag::snapshot().get(Tag::Api).heap
        };
        let (counted, committed) = (heap_of_api(), committed_now());
        let kept = blaze_mem::frame::task(Tag::Api, |_| {
            blaze_graph::arena::heap_dies_with_task(|| {
                let blocks: Vec<usize> = (0..BLOCKS)
                    .map(|_| {
                        // SAFETY: a valid layout; the block is left to its heap's destruction.
                        let block = unsafe { COUNTED.alloc(layout(BLOCK)) };
                        assert!(!block.is_null());
                        // SAFETY: the block is `BLOCK` bytes, ours.
                        unsafe { block.write_bytes(1, BLOCK) };
                        // SAFETY: `block` is a live block of this allocator, just allocated.
                        let holder = unsafe { Heaps.holder(block) };
                        assert_eq!(holder.tag, Some(Tag::Api));
                        assert!(holder.task.place().is_some(), "in the task's own heap");
                        block.addr()
                    })
                    .collect();
                assert!(committed_now() >= committed + (BLOCK * BLOCKS) as u64 - SLACK);
                blocks.len()
            })
        });
        assert_eq!(kept, BLOCKS);
        assert_eq!(heap_of_api(), counted, "the dead heap's blocks left its tag's line");
        let after = committed_now();
        assert!(after <= committed + SLACK, "committed went from {committed} to {after}");

        // The control: a heap left open.
        let leaked = open_task_heap(Tag::Api).expect("a place in the pool");
        for _ in 0..BLOCKS {
            // SAFETY: a valid layout; the block dies with the heap below.
            let block = unsafe { Heaps.alloc_for(Holder::in_task(leaked), layout(BLOCK), false) };
            assert!(!block.is_null());
            // SAFETY: the block is `BLOCK` bytes, ours.
            unsafe { block.write_bytes(1, BLOCK) };
        }
        let held = committed_now();
        assert!(
            held >= committed + (BLOCK * BLOCKS) as u64 - SLACK,
            "control: the open heap holds its blocks, yet committed went from {committed} to {held}"
        );
        close_task_heap(leaked);
        assert!(committed_now() <= committed + SLACK, "closing the control gives its bytes back");
    }

    /// A value kept past its task is caught in a test build: a block made in a task's own heap,
    /// its address kept past the task's end, faults when read (a child process reads it and
    /// dies of the fault), and [`kept_past_task`] names the task's tag there. The control: a
    /// block of the same size in the tag's heap, kept and still live, reads in the child.
    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn a_value_kept_past_its_task_faults_naming_the_task() {
        unsafe extern "C" {
            fn fork() -> c_int;
            fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
            fn _exit(status: c_int) -> !;
        }
        /// How a child that reads `addr` ends: `Ok(exit code)` or `Err(signal)`.
        fn read_in_child(addr: usize) -> Result<c_int, c_int> {
            // SAFETY: the child only reads one byte and exits, allocating nothing.
            let pid = unsafe { fork() };
            assert!(pid >= 0, "fork");
            if pid == 0 {
                // SAFETY: the read under test; a fault ends the child.
                let byte = unsafe { ptr::with_exposed_provenance::<u8>(addr).read_volatile() };
                // SAFETY: ends the child at once.
                unsafe { _exit(c_int::from(byte)) }
            }
            let mut status = 0;
            // SAFETY: our child; `status` is a live local.
            assert_eq!(unsafe { waitpid(pid, &raw mut status, 0) }, pid);
            match status & 0x7f {
                0 => Ok((status >> 8) & 0xff),
                signal => Err(signal),
            }
        }
        const BLOCK: usize = 256 << 10;
        let _measuring = MEASURING.lock();
        ready();
        let kept = blaze_mem::frame::task(Tag::Api, |_| {
            blaze_graph::arena::heap_dies_with_task(|| {
                // SAFETY: a valid layout; the block dies with the task's heap.
                let block = unsafe { COUNTED.alloc(layout(BLOCK)) };
                assert!(!block.is_null());
                // SAFETY: the block is `BLOCK` bytes, ours.
                unsafe { block.write_bytes(7, BLOCK) };
                block.expose_provenance() + BLOCK / 2
            })
        });
        let signal = read_in_child(kept).expect_err("the kept block was read after its task");
        assert!(matches!(signal, 10 | 7 | 11), "the child died of signal {signal}, not a fault");
        assert_eq!(kept_past_task(kept).map(|it| it.tag), Some(Tag::Api));

        // SAFETY: a valid layout; freed below, with it.
        let live = unsafe { Heaps.alloc_for(Holder::of(Tag::Api), layout(BLOCK), false) };
        // SAFETY: the block is `BLOCK` bytes, ours.
        unsafe { live.write_bytes(7, BLOCK) };
        assert_eq!(read_in_child(live.expose_provenance()), Ok(7), "control: a live block reads");
        assert_eq!(kept_past_task(live.addr()), None);
        // SAFETY: made above with this layout, freed once.
        unsafe { Heaps.dealloc(live, layout(BLOCK)) };
    }

    /// What a thread freed of a tag's heap mimalloc keeps until it collects; a give-back
    /// collects it as the thread settles and the purge takes it out of the footprint at once.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_give_back_purges_the_pages_a_thread_freed_at_once() {
        let _measuring = MEASURING.lock();
        const BLOCK: usize = 4 << 10;
        const BLOCKS: usize = 64 << 10; // 256 MiB
        let (freed, asked, settled) =
            (std::sync::Barrier::new(2), std::sync::Barrier::new(2), std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut blocks = Vec::with_capacity(BLOCKS);
                for _ in 0..BLOCKS {
                    // SAFETY: a valid layout; each block is freed below, once, with it.
                    let block =
                        unsafe { Heaps.alloc_for(Holder::of(Tag::Parse), layout(BLOCK), false) };
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
                blaze_cargo::raw::memory::own_footprint_now().expect("the footprint is readable");
            blaze_mem::tag::give_back_heap();
            asked.wait();
            settled.wait();
            let after =
                blaze_cargo::raw::memory::own_footprint_now().expect("the footprint is readable");
            let given = before.saturating_sub(after);
            assert!(
                given >= (BLOCK * BLOCKS / 2) as u64,
                "the give-back took the footprint from {before} to {after}: {given} bytes of the {} the freed blocks held",
                BLOCK * BLOCKS,
            );
        });
    }

    /// The page walk names a tag's blocks where they are: blocks made for a tag show in its
    /// pages as committed and used bytes of at least their sizes, and once they are freed and
    /// the thread collected, the tag's used bytes fall by them. On a tag no other test of this
    /// binary allocates for, on one thread. The cause, by removing it: a walk that read
    /// another field for `used` (mimalloc 2's `mi_heap_area_t` has no `full_block_size`)
    /// fails the first bound.
    #[test]
    fn the_page_walk_counts_a_tags_live_blocks_and_sees_them_go() {
        let _measuring = MEASURING.lock();
        const BLOCK: usize = 1 << 10;
        const BLOCKS: usize = 4 << 10; // 4 MiB
        let of = |tag: Tag| {
            let mut all = [blaze_mem::tag::HeapPages::default(); Tag::COUNT + 1];
            pages(&mut all);
            all[tag.index()]
        };
        std::thread::spawn(move || {
            let before = of(Tag::Fuzz);
            let blocks: Vec<Sent> = (0..BLOCKS)
                .map(|_| {
                    // SAFETY: a valid layout; each block is freed below, once, with it.
                    let block =
                        unsafe { Heaps.alloc_for(Holder::of(Tag::Fuzz), layout(BLOCK), false) };
                    assert!(!block.is_null());
                    Sent(block, ptr::null_mut())
                })
                .collect();
            let held = of(Tag::Fuzz);
            let made = (BLOCK * BLOCKS) as u64;
            assert!(held.used >= before.used + made, "{before:?} then {held:?}");
            assert!(held.committed >= held.used && held.pages > before.pages, "{held:?}");
            for block in blocks {
                // SAFETY: a live block made above with this layout, freed once.
                unsafe { Heaps.dealloc(block.0, layout(BLOCK)) };
            }
            give_back_here(blaze_mem::tag::heap_give_backs());
            let after = of(Tag::Fuzz);
            assert!(after.used + made <= held.used, "{held:?} then {after:?}");
        })
        .join()
        .expect("the walking thread");
    }

    /// The purge before a refusal or at the ceiling (`blaze_cargo::raw::memory::purge`) reads
    /// the footprint only once a counted worker gave back what it kept, and that takes it out
    /// of the footprint: large and small blocks a worker made and touched, freed on another
    /// thread (so into the worker's own pages, which only it may collect), leave the footprint
    /// by most of their bytes between the purge's two readings. The worker looks for a
    /// give-back every 20 ms, as an idle one woken late would: a purge that read the footprint
    /// at once, as before 2026-11-09, reads it with the blocks still in it, and this fails
    /// (test-the-cause: set `SETTLE_WITHIN` to 0).
    #[cfg(target_os = "macos")]
    #[test]
    fn a_purge_reads_the_footprint_after_a_worker_gave_back_what_another_thread_freed() {
        let _measuring = MEASURING.lock();

        use blaze_core::wait::{Clock as _, Deadline, Monotonic};

        const LARGE: usize = 32 << 20;
        const LARGES: usize = 4; // 128 MiB
        const SMALL: usize = 4 << 10;
        const SMALLS: usize = 32 << 10; // 128 MiB
        let larges: Vec<AtomicPtr<u8>> =
            (0..LARGES).map(|_| AtomicPtr::new(ptr::null_mut())).collect();
        let smalls: Vec<AtomicPtr<u8>> =
            (0..SMALLS).map(|_| AtomicPtr::new(ptr::null_mut())).collect();
        // The tag's heap made first, so the allocator's give-back is set (`heap_gives_back_by`)
        // before the worker counts: in the tests mimalloc's heaps are no global allocator.
        // SAFETY: a valid layout; the block is freed at once, with it.
        unsafe {
            Heaps.dealloc(Heaps.alloc_for(Holder::of(Tag::Parse), layout(SMALL), false), layout(SMALL));
        }
        let made = std::sync::Barrier::new(2);
        let stop = AtomicBool::new(false);
        let purged = std::thread::scope(|scope| {
            scope.spawn(|| {
                // A pool worker's count: the purge waits for this thread's give-back.
                let _settles = blaze_mem::tag::count_heap_settles();
                for (slots, size) in [(&larges, LARGE), (&smalls, SMALL)] {
                    for slot in slots {
                        // SAFETY: a valid layout; the block is freed once, with it, below.
                        let block =
                            unsafe { Heaps.alloc_for(Holder::of(Tag::Parse), layout(size), false) };
                        assert!(!block.is_null());
                        // Touched, so its pages are resident and in the footprint.
                        // SAFETY: the block is `size` bytes, ours.
                        unsafe { block.write_bytes(1, size) };
                        slot.store(block, Ordering::Release);
                    }
                }
                made.wait();
                // An idle worker that looks late: it gives back once asked, at its next look.
                let clock = Monotonic::new();
                while !stop.load(Ordering::Acquire) {
                    blaze_mem::tag::settle_heap();
                    let next = Deadline::after(clock.now(), std::time::Duration::from_millis(20));
                    blaze_core::wait::sleep_until("the test worker's next look", next, &clock)
                        .expect("the test worker sleeps");
                }
            });
            made.wait();
            for (slots, size) in [(&larges, LARGE), (&smalls, SMALL)] {
                for slot in slots {
                    // SAFETY: a live block the worker made with this layout, freed once.
                    unsafe { Heaps.dealloc(slot.load(Ordering::Acquire), layout(size)) };
                }
            }
            let purged = blaze_cargo::raw::memory::purge();
            stop.store(true, Ordering::Release);
            purged
        });
        assert!(
            matches!(purged.settle, blaze_mem::tag::Settle::Settled { .. }),
            "the purge read the footprint before the worker gave back: {purged:?}"
        );
        let freed = (LARGE * LARGES + SMALL * SMALLS) as u64;
        assert!(
            purged.returned() >= freed / 2,
            "the purge returned {} bytes of the {freed} the worker's freed blocks held: {purged:?}",
            purged.returned(),
        );
    }

    /// What [`freed_by_another`] saw: the arena's committed bytes before the blocks were made
    /// and once the give-back settled or its wait ran out, and where the give-back stood.
    struct Committed {
        start: u64,
        end: u64,
        settle: blaze_mem::tag::Settle,
    }

    /// Four counted threads each make and touch 64 MiB of blocks (256 MiB in all), this thread
    /// frees every one of them (so into the makers' own pages, which only each maker may
    /// collect), asks a give-back and waits `within` for it to settle. The first `settling`
    /// makers look for a give-back every millisecond, as an idle worker woken for it does; the
    /// rest never do.
    fn freed_by_another(settling: usize, within: std::time::Duration) -> Committed {
        use blaze_core::wait::{Clock as _, Deadline, Monotonic};

        const MAKERS: usize = 4;
        const BLOCK: usize = 16 << 10;
        const EACH: usize = (64 << 20) / BLOCK;
        let blocks: Vec<AtomicPtr<u8>> =
            (0..MAKERS * EACH).map(|_| AtomicPtr::new(ptr::null_mut())).collect();
        let made = std::sync::Barrier::new(MAKERS + 1);
        let stop = AtomicBool::new(false);
        let start = committed_now();
        std::thread::scope(|scope| {
            for maker in 0..MAKERS {
                let (blocks, made, stop) = (&blocks, &made, &stop);
                scope.spawn(move || {
                    // A pool worker's count: the give-back settles only once this thread gave.
                    let _settles = blaze_mem::tag::count_heap_settles();
                    for slot in &blocks[maker * EACH..(maker + 1) * EACH] {
                        // SAFETY: a valid layout; the block is freed once, with it, below.
                        let block = unsafe {
                            Heaps.alloc_for(Holder::of(Tag::Parse), layout(BLOCK), false)
                        };
                        assert!(!block.is_null());
                        // Touched, so its pages are committed and resident.
                        // SAFETY: the block is `BLOCK` bytes, ours.
                        unsafe { block.write_bytes(1, BLOCK) };
                        slot.store(block, Ordering::Release);
                    }
                    made.wait();
                    let clock = Monotonic::new();
                    while !stop.load(Ordering::Acquire) {
                        if maker < settling {
                            blaze_mem::tag::settle_heap();
                        }
                        let next = Deadline::after(clock.now(), std::time::Duration::from_millis(1));
                        blaze_core::wait::sleep_until("the test maker's next look", next, &clock)
                            .expect("the test maker sleeps");
                    }
                });
            }
            made.wait();
            for slot in &blocks {
                // SAFETY: a live block a maker made with this layout, freed once.
                unsafe { Heaps.dealloc(slot.load(Ordering::Acquire), layout(BLOCK)) };
            }
            let asked = blaze_mem::tag::ask_heap_give_back();
            let clock = Monotonic::new();
            let deadline = Deadline::after(clock.now(), within);
            let settle = loop {
                let settle = blaze_mem::tag::heap_give_back_settled();
                let settled = matches!(
                    settle,
                    blaze_mem::tag::Settle::Settled { generation } if generation >= asked
                );
                if settled || deadline.passed(clock.now()) {
                    break settle;
                }
                let next = Deadline::after(clock.now(), std::time::Duration::from_millis(1));
                blaze_core::wait::sleep_until("the test's next look at the give-back", next, &clock)
                    .expect("the test sleeps");
            };
            // The purge after every maker, as `blaze_cargo::raw::memory::purge` does (its step
            // 3): a maker's own forced purge skips every arena while another maker holds
            // mimalloc's purge guard, so the pages it gave back are reached only by a purge
            // that begins after the last one gave.
            // SAFETY: `mi_collect` reads and writes no memory of ours; callable from any thread.
            unsafe { mi_collect(true) };
            let end = committed_now();
            stop.store(true, Ordering::Release);
            Committed { start, end, settle }
        })
    }

    /// The invariant: once every thread that holds pages of freed blocks gave back for a
    /// give-back asked after the frees, and the arenas were purged after the last of them (the
    /// daemon's purge, `blaze_cargo::raw::memory::purge`, steps 2 and 3), [`committed_now`] is
    /// within 8 MiB of what it was before the blocks were made: 256 MiB made on four threads
    /// and freed on a fifth leave the arena's count. The control, by removing the cause: with
    /// one maker that never gives back the give-back never settles and its 64 MiB part stays
    /// committed.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_freed_heap_is_no_longer_committed_after_every_thread_settles() {
        const SLACK: u64 = 8 << 20;
        let _measuring = MEASURING.lock();
        // The tag's heap made first, so the allocator's give-back is set
        // (`heap_gives_back_by`) before the makers count.
        // SAFETY: a valid layout; the block is freed at once, with it.
        unsafe {
            Heaps.dealloc(Heaps.alloc_for(Holder::of(Tag::Parse), layout(64), false), layout(64));
        }
        let all = freed_by_another(4, std::time::Duration::from_secs(10));
        assert!(
            matches!(all.settle, blaze_mem::tag::Settle::Settled { .. }),
            "every maker gave back, yet the give-back did not settle: {:?}",
            all.settle,
        );
        assert!(
            all.end <= all.start + SLACK,
            "the arena held {} bytes before the blocks and {} once every thread gave back: \
             {} more, past the {SLACK} allowed",
            all.start,
            all.end,
            all.end.saturating_sub(all.start),
        );
        let control = freed_by_another(3, std::time::Duration::from_millis(500));
        assert!(
            matches!(control.settle, blaze_mem::tag::Settle::Pending { .. }),
            "control: a maker never gave back, yet the give-back settled: {:?}",
            control.settle,
        );
        assert!(
            control.end > control.start + SLACK,
            "control: the maker that never gave back still holds its part, yet committed went \
             from {} to {}",
            control.start,
            control.end,
        );
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
