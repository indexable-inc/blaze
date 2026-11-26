//! The root heap: the one allocator a program's entry, or a test, MAKES at its top from the
//! operating system's pages and hands down (decision D102, rule 75 "nothing ambient";
//! docs/v2/plan/design/no-global-heap.md, "the world's region: one blaze_mem region made by rt at
//! entry from the page source"). Every [`Alloc`] and [`SharedAlloc`] it gives is borrowed from it
//! ([`Root::alloc`], [`Root::shared`]), so nothing allocates without having been handed one, and
//! nothing it made outlives it: dropping the root gives every page back to the kernel.
//!
//! It lives here, not in blaze-own: blaze-own is `core` alone and has no page source; the root's
//! pages are one [`Region`], counted to the tag its maker names like every other region's.
//!
//! # How it allocates
//!
//! One reservation of [`RootOpts::reserve`] bytes, committed as it is carved (the region's
//! geometric commits). A block's size is rounded up to its size class: 16 to 128 bytes by 16,
//! then four classes to each doubling (160, 192, 224, 256, 320, ...) up to [`LARGEST`]. Every
//! block of a class is aligned to the largest power of two dividing the class's size, up to
//! [`MAX_ALIGN`]; a layout asking for more alignment than its size's class has takes the next
//! class that has it, and one asking for more than [`MAX_ALIGN`] is refused.
//!
//! - A take pops the class's free list, else carves the next block from the reservation's end
//!   (one compare-and-swap on the end, and a commit system call when the region grows).
//! - A give pushes the block on its class's free list, except the block that ends the carved
//!   part: that one is uncarved (the end moves back), so a stack-like use reuses the same bytes.
//! - A growth or shrink stays in place when the new size is in the same class, or when the block
//!   ends the carved part (the end moves); else it moves (take, copy, give).
//!
//! A free list is intrusive (each free block's first word holds the next one's offset) and
//! guarded by a lock of its own, a word taken by compare-and-swap; what it guards is two loads
//! and two stores, so a waiter spins, then yields, and a lock held through [`YIELDS`] yields
//! stops the process naming the class (rule 18: a holder inside that section cannot block, so
//! that is a bug, never a wait). The lock is the root's own word: no `std` lock, whose macOS form
//! boxes its mutex in the global heap.
//!
//! # Memory: what a give does and the waste
//!
//! A given block keeps its pages and is reused only by its class; nothing is returned to the
//! kernel until the root drops (a root lives as long as a program or a test: rule 70). So:
//!
//! - rounding: a block wastes at most 15 bytes at 128 bytes and below, and under a fifth of
//!   itself above (the step to the next class is a quarter of the doubling below it);
//! - padding: a carve skips at most its class's alignment less 16 bytes to align the block (a
//!   16-byte take after a 160-byte one skips nothing; a page-aligned one at most a page);
//! - free blocks: committed bytes are at most the sum over classes of each class's peak live
//!   bytes, plus the two above. A `Vec` doubled to `n` bytes alone at the end grows in place (no
//!   copy, nothing left behind); doubled while other blocks are carved after it, it leaves one
//!   free block of each smaller class, under `n` bytes in all, which the next vector of those
//!   sizes takes.
//!
//! # Determinism
//!
//! Offsets within the root ([`Root::offset_of`]) are a function of the sequence of calls on one
//! thread: the free lists are last in, first out and the carve is in order, and the reservation's
//! base is aligned to at least [`MAX_ALIGN`], so alignment does not depend on where the kernel put
//! it. Threads that allocate at once get offsets in the order they won the end or a list: a
//! value keyed by an address must not be (rule 25).
//!
//! # Counting
//!
//! [`Root::used`]: blocks and bytes live (the layouts' sizes), takes in all, bytes carved and
//! committed. In a debug build (every test) a root dropped with a block still live panics naming
//! the count: a leak is a test's failure.

use core::{
    alloc::{AllocError, Allocator, Layout},
    cell::Cell,
    fmt, hint,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU8, AtomicUsize, Ordering},
};
use std::io;

use blaze_own::{Alloc, Headed, Header, SharedAlloc};

use crate::{MemError, Region, Tag};

/// The smallest block, and the unit of the small classes.
pub const GRAIN: usize = 16;

/// The small classes: [`GRAIN`] to [`SMALL_MAX`] by [`GRAIN`].
const SMALL_CLASSES: usize = SMALL_MAX / GRAIN;
const SMALL_MAX: usize = 128;
const SMALL_SHIFT: u32 = SMALL_MAX.trailing_zeros();

/// Classes to each doubling above [`SMALL_MAX`].
const STEPS: usize = 4;

/// The doubling of the largest class's base: its size is twice this power of two (128 TiB on a
/// 64-bit target, 1 GiB on a 32-bit one).
const TOP_SHIFT: u32 = if usize::BITS >= 64 { 46 } else { 29 };

/// How many size classes a root has.
pub const CLASSES: usize = SMALL_CLASSES + STEPS * (TOP_SHIFT - SMALL_SHIFT + 1) as usize;

/// The largest block a root gives.
pub const LARGEST: usize = class_size(CLASSES - 1);

/// The largest alignment a root gives: a block asking for more is refused. A reservation's base
/// is aligned to at least this on every system (a region's granule is 64 KiB).
pub const MAX_ALIGN: usize = 64 << 10;

/// A list's lock is spun on this many times before each yield.
const SPINS: u32 = 64;

/// The yields a lock may be waited through before the wait is a bug (rule 18): at a microsecond
/// or more each, seconds, against a section of four memory operations.
pub const YIELDS: u32 = 1 << 22;

const _: () = assert!(SMALL_MAX.is_power_of_two() && SMALL_MAX % GRAIN == 0);
const _: () = assert!(GRAIN >= size_of::<usize>() && GRAIN.is_power_of_two());
const _: () = assert!(class_size(SMALL_CLASSES) == SMALL_MAX + SMALL_MAX / STEPS);
const _: () = assert!(LARGEST == 2 << TOP_SHIFT);

/// The size of class `index`.
pub const fn class_size(index: usize) -> usize {
    if index < SMALL_CLASSES {
        return GRAIN * (index + 1);
    }
    let doubling = (index - SMALL_CLASSES) / STEPS;
    let step = (index - SMALL_CLASSES) % STEPS + 1;
    let base = SMALL_MAX << doubling;
    base + step * (base / STEPS)
}

/// The alignment every block of class `index` has: the largest power of two dividing its size,
/// at most [`MAX_ALIGN`].
pub const fn class_align(index: usize) -> usize {
    let size = class_size(index);
    let align = size & size.wrapping_neg();
    if align > MAX_ALIGN { MAX_ALIGN } else { align }
}

/// The smallest class of at least `size` bytes; `None` past [`LARGEST`]. A size of 0 is the
/// smallest class's.
pub const fn class_of(size: usize) -> Option<usize> {
    if size <= SMALL_MAX {
        return Some(if size == 0 { 0 } else { (size - 1) / GRAIN });
    }
    if size > LARGEST {
        return None;
    }
    // `base < size <= 2 * base`, `base` a power of two at least `SMALL_MAX`.
    let shift = usize::BITS - 1 - (size - 1).leading_zeros();
    let base = 1usize << shift;
    let step = (size - base).div_ceil(base / STEPS);
    Some(SMALL_CLASSES + STEPS * (shift - SMALL_SHIFT) as usize + (step - 1))
}

/// Why a root refused a take (kept as [`Root::refusal`] says).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// The reservation has no room for `wanted` more bytes.
    Exhausted { wanted: usize },
    /// The kernel refused to commit the pages up to `bytes` (`errno` 0 when it gave none).
    Commit { bytes: usize, errno: i32 },
    /// A block of `wanted` bytes is past [`LARGEST`].
    TooLarge { wanted: usize },
    /// An alignment past [`MAX_ALIGN`].
    Align { wanted: usize },
}

impl Refused {
    const NONE: u8 = 0;
    const EXHAUSTED: u8 = 1;
    const COMMIT: u8 = 2;
    const TOO_LARGE: u8 = 3;
    const ALIGN: u8 = 4;

    fn of_mem(error: &MemError, wanted: usize) -> Refused {
        match error {
            MemError::Exhausted { .. } => Refused::Exhausted { wanted },
            MemError::TooLarge { .. } => Refused::TooLarge { wanted },
            MemError::Commit { bytes, os, .. } => Refused::Commit { bytes: *bytes, errno: os.raw_os_error().unwrap_or(0) },
            MemError::Reserve { os, .. } | MemError::Evict { os, .. } => {
                Refused::Commit { bytes: wanted, errno: os.raw_os_error().unwrap_or(0) }
            }
        }
    }

    /// The error it is, for a root reserving `reserved` bytes for `tag`.
    fn error(self, tag: Tag, reserved: usize) -> MemError {
        match self {
            Refused::Exhausted { wanted } => MemError::Exhausted { tag, reserved, wanted },
            Refused::Commit { bytes, errno } => MemError::Commit { tag, bytes, os: io::Error::from_raw_os_error(errno) },
            Refused::TooLarge { wanted } => MemError::TooLarge { tag, what: "a block of a root (its largest class)", wanted },
            Refused::Align { wanted } => MemError::TooLarge { tag, what: "a block's alignment in a root (at most 64 KiB)", wanted },
        }
    }
}

/// The class a block of `layout` is in.
fn class_for(layout: Layout) -> Result<usize, Refused> {
    if layout.align() > MAX_ALIGN {
        return Err(Refused::Align { wanted: layout.align() });
    }
    let mut class = class_of(layout.size()).ok_or(Refused::TooLarge { wanted: layout.size() })?;
    // Bounded: at most `CLASSES` steps; a class whose size is a multiple of the alignment comes
    // within a doubling of the size, and `MAX_ALIGN` divides every class from 64 KiB on.
    while class_align(class) < layout.align() {
        class += 1;
        if class == CLASSES {
            return Err(Refused::TooLarge { wanted: layout.size() });
        }
    }
    Ok(class)
}

/// What a root makes itself from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootOpts {
    /// The address space it reserves, rounded up to whole granules: its most, never moved or
    /// grown. Reserving commits nothing; pages are committed as blocks are carved.
    pub reserve: usize,
    /// The tag its pages count to (`tag::snapshot`, the profile's memory rows).
    pub tag: Tag,
    /// What a block still live when the root drops is: a choice its maker states (no default).
    pub live_at_drop: LiveAtDrop,
}

/// What a block still live at a root's drop is ([`RootOpts::live_at_drop`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveAtDrop {
    /// A leak: in a debug build the drop panics naming the count. For code that gives back
    /// every block it takes.
    Refused,
    /// The root's to free: code that keeps values for the root's whole life by design
    /// (`Vec::leak`, `Box::leak` into the handed allocator: how a value is made to live as long
    /// as its arena) leaves them, and the drop unmaps them with everything else. A test of such
    /// code that wants a bound reads [`Root::used`]. (The first three test files ported to a
    /// root, 2026-11-17, were of such code: the names resolution and a crate's environment.)
    Freed,
}

impl RootOpts {
    /// A test's root: 16 GiB of address space (nothing of it committed until used), counted to
    /// [`Tag::Other`], a block live at its drop a leak. Made at the test's top:
    /// `let root = Root::new(RootOpts::TEST)?;`.
    #[cfg(target_pointer_width = "64")]
    pub const TEST: RootOpts = RootOpts { reserve: 16 << 30, tag: Tag::Other, live_at_drop: LiveAtDrop::Refused };

    /// A test's root on a 32-bit target, where a reservation is memory taken whole: 256 MiB.
    #[cfg(not(target_pointer_width = "64"))]
    pub const TEST: RootOpts = RootOpts { reserve: 256 << 20, tag: Tag::Other, live_at_drop: LiveAtDrop::Refused };

    /// [`RootOpts::TEST`] for code that keeps values to its arena's end ([`LiveAtDrop::Freed`]).
    pub const TEST_ARENA: RootOpts = RootOpts { live_at_drop: LiveAtDrop::Freed, ..RootOpts::TEST };
}

/// What a root holds now ([`Root::used`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootUsed {
    /// Blocks given out and not given back.
    pub live_blocks: usize,
    /// Their bytes, as their layouts say (not rounded to their classes).
    pub live_bytes: usize,
    /// Takes in all, a growth or shrink that moved its block among them: "this call allocated
    /// nothing" is this unchanged across it.
    pub allocations: usize,
    /// Bytes of the reservation carved into blocks (live or free) and padding.
    pub carved: usize,
    /// Bytes committed.
    pub committed: usize,
    /// Bytes reserved.
    pub reserved: usize,
}

/// Cache-line padded wrapper (128 bytes for ARM64/x86-64 spatial prefetcher pairs, rule 86)
/// so hot atomics and spinlocks never false-share across cores on 128-core machines.
#[repr(align(128))]
struct Padded<T>(T);

/// Number of 128-byte cache-padded per-thread shards owned inside each [`Root`]: 1 shard per core
/// on 128-core Graviton 5 / EPYC hosts, eliminating global spinlock, `carved` CAS, and counter
/// contention with zero TLS destructor races or dangling pointers on `Root::drop`.
const SHARDS: usize = 128;

/// Small size classes (`16B..=512B`, classes `0..16`) cached in each shard's local magazine.
const LOCAL_CLASSES: usize = 16;

/// Per-shard, per-class block count at which [`LOCAL_FLUSH`] blocks are spliced back to the
/// shared class list in one lock acquisition.
const LOCAL_HIGH_WATER: u8 = 64;

/// Blocks moved from a full shard magazine to the shared class list in one O(1) splice.
const LOCAL_FLUSH: u8 = 32;

/// Blocks carved or refilled per batch in multi-threaded mode for small class `class` (~4 KiB
/// per batch, bounded to `8..=32` blocks).
const fn batch_size(class: usize) -> usize {
    let size = class_size(class);
    let n = 4096 / size;
    if n < 8 {
        8
    } else if n > 32 {
        32
    } else {
        n
    }
}

#[inline]
fn thread_shard() -> usize {
    crate::frame::thread_span_shard() & (SHARDS - 1)
}

/// Bounded spinlock shared by [`List`] and [`Shard`]: guards a few pointer operations, never blocks,
/// and stops naming `class` past [`YIELDS`] yields (rules 12, 18, 30).
#[expect(
    clippy::disallowed_methods,
    reason = "the root heap is below blaze_graph and its spin-then-park helper; the section it guards is a few memory operations, and the spin is bounded by YIELDS, past which it stops naming the class (rules 12 and 18)"
)]
fn acquire_spin(held: &AtomicBool, class: usize, tag: Tag) -> SpinGuard<'_> {
    let mut yields = 0u32;
    loop {
        // Acquire: what the last holder wrote of the list (a block's link) is seen here.
        if held.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return SpinGuard { held };
        }
        let mut spins = 0u32;
        while held.load(Ordering::Relaxed) {
            if spins < SPINS {
                spins += 1;
                hint::spin_loop();
                continue;
            }
            yields += 1;
            if yields > YIELDS {
                stuck(class, tag);
            }
            std::thread::yield_now();
        }
    }
}

/// A held spinlock guard, given back when dropped.
struct SpinGuard<'l> {
    held: &'l AtomicBool,
}

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        // Release: what this holder wrote is seen by the next.
        self.held.store(false, Ordering::Release);
    }
}

/// One class's shared free blocks, padded to its own 128-byte cache line so concurrent refill/flush
/// across adjacent size classes never false-share spinlocks or free-list heads.
#[repr(align(128))]
struct List {
    /// Its lock (see the module).
    held: AtomicBool,
    /// The offset of the top free block plus one; 0 when there is none. Written only with
    /// `held`; read without it only as a hint that the list is empty.
    top: AtomicUsize,
}

impl List {
    const EMPTY: List = List { held: AtomicBool::new(false), top: AtomicUsize::new(0) };

    fn lock(&self, class: usize, tag: Tag) -> SpinGuard<'_> {
        acquire_spin(&self.held, class, tag)
    }
}

/// One thread-slot shard owned inside [`Root`], aligned to 128 bytes (256 bytes total = two
/// cache-line pairs) so each core on a 128-core machine allocates, frees, and accounts small
/// blocks (`16B..=512B`) on its own private cache lines with zero cross-core contention.
#[repr(align(128))]
struct Shard {
    /// Signed net block delta (`+1` on allocate, `-1` on deallocate); summed across shards for
    /// exact [`RootUsed::live_blocks`] and leak detection even across cross-thread frees.
    live_blocks: AtomicIsize,
    /// Signed net byte delta; summed across shards for exact [`RootUsed::live_bytes`].
    live_bytes: AtomicIsize,
    /// Cumulative allocations on this shard; summed across shards for [`RootUsed::allocations`].
    allocations: AtomicUsize,
    /// Uncontended per-shard spinlock protecting `counts` and `heads` (only contended if >128
    /// threads share a shard or another shard steals on exhaustion).
    held: AtomicBool,
    /// Cached block count per small class (`0..LOCAL_CLASSES`).
    counts: [AtomicU8; LOCAL_CLASSES],
    /// Intrusive LIFO free-list head (`offset + 1`, or `0` if empty) per small class.
    heads: [AtomicUsize; LOCAL_CLASSES],
}

impl Shard {
    const EMPTY: Shard = Shard {
        live_blocks: AtomicIsize::new(0),
        live_bytes: AtomicIsize::new(0),
        allocations: AtomicUsize::new(0),
        held: AtomicBool::new(false),
        counts: [const { AtomicU8::new(0) }; LOCAL_CLASSES],
        heads: [const { AtomicUsize::new(0) }; LOCAL_CLASSES],
    };

    fn lock(&self, class: usize, tag: Tag) -> SpinGuard<'_> {
        acquire_spin(&self.held, class, tag)
    }
}

#[cold]
#[inline(never)]
fn stuck(class: usize, tag: Tag) -> ! {
    panic!(
        "blaze_mem::Root ({tag}): the free list of its {}-byte class stayed locked through {YIELDS} yields; its holder \
         stopped inside a four-operation section, which cannot block: a bug (rule 18)",
        class_size(class)
    )
}

/// Cold error-reporting slots for [`Root::refusal`], grouped on their own 128-byte cache line
/// so cold refusal state never shares a line with hot allocation atomics or free lists.
struct RefusalSlots {
    kind: AtomicU8,
    wanted: AtomicUsize,
    errno: AtomicI32,
}

/// The root heap (see the module): made once at a program's or a test's top, handed down as
/// `&Root`, [`Alloc`] or [`SharedAlloc`].
///
/// `#[repr(C)]` with its [`Header`] first, so a handle is its address ([`Headed`]).
/// Per-thread-slot [`Shard`] magazines, [`carved`], cold [`refusal`], and every per-class
/// [`List`] spinlock are isolated onto distinct 128-byte cache lines (rule 86) to eliminate
/// cross-core false sharing on 128-core ARM64/x86-64 machines.
#[repr(C)]
pub struct Root {
    /// First: the handle's table (blaze-own's invariant 1).
    header: Header,
    region: Region,
    /// The maker's choice for blocks live at the drop ([`RootOpts::live_at_drop`]).
    live_at_drop: LiveAtDrop,
    /// First shard to allocate from this root (`usize::MAX` until claimed); when a second shard
    /// uses the root, `shared_mode` flips to `true` and enables multi-block batch carving.
    owner_shard: AtomicUsize,
    shared_mode: AtomicBool,
    /// The end of the carved part, an offset into the reservation (hot CAS on carve/uncarve).
    carved: Padded<AtomicUsize>,
    /// The last refusal, for [`Root::refusal`]: its kind ([`Refused`]'s codes), the size it
    /// named and the kernel's error number. Written by the refusing thread, read whole only by
    /// the caller it refused (another thread's refusal at once may mix the fields).
    refusal: Padded<RefusalSlots>,
    lists: [List; CLASSES],
    shards: [Shard; SHARDS],
}

const _: () = assert!(core::mem::offset_of!(Root, header) == 0);
const _: () = assert!(align_of::<List>() == 128);
const _: () = assert!(align_of::<Shard>() == 128);
const _: () = assert!(size_of::<Shard>() == 256);
const _: () = assert!(align_of::<Root>() == 128);
const _: () = assert!(core::mem::offset_of!(Root, carved) % 128 == 0);
const _: () = assert!(core::mem::offset_of!(Root, refusal) % 128 == 0);
const _: () = assert!(core::mem::offset_of!(Root, lists) % 128 == 0);
const _: () = assert!(core::mem::offset_of!(Root, shards) % 128 == 0);
const _: () = assert!(size_of::<Root>() <= 64 << 10);
const _: () = assert!(SHARDS.is_power_of_two() && LOCAL_CLASSES <= CLASSES);
const _: () = assert!(LOCAL_FLUSH < LOCAL_HIGH_WATER);

impl Root {
    /// A root reserving `opts.reserve` bytes of address space for `opts.tag`, committing nothing
    /// until its first block. Errs when the kernel refuses the reservation.
    pub fn new(opts: RootOpts) -> Result<Root, MemError> {
        let region = Region::reserve(opts.reserve, opts.tag)?;
        assert!(
            crate::align::is_aligned(region.base().addr().get(), MAX_ALIGN),
            "blaze_mem::Root ({}): the kernel's reservation at {:p} is not aligned to {MAX_ALIGN} bytes, which every \
             region's granule is: offsets would not be aligned as addresses",
            opts.tag,
            region.base()
        );
        Ok(Root {
            header: Header::of::<Root>(),
            region,
            live_at_drop: opts.live_at_drop,
            owner_shard: AtomicUsize::new(usize::MAX),
            shared_mode: AtomicBool::new(false),
            carved: Padded(AtomicUsize::new(0)),
            refusal: Padded(RefusalSlots {
                kind: AtomicU8::new(Refused::NONE),
                wanted: AtomicUsize::new(0),
                errno: AtomicI32::new(0),
            }),
            lists: [const { List::EMPTY }; CLASSES],
            shards: [const { Shard::EMPTY }; SHARDS],
        })
    }

    /// The handle every function takes, borrowed from the root.
    #[inline]
    pub fn alloc(&self) -> Alloc<'_> {
        Alloc::of(self)
    }

    /// The handle that may cross threads, borrowed from the root (it is `Sync`).
    #[inline]
    pub fn shared(&self) -> SharedAlloc<'_> {
        SharedAlloc::of(self)
    }

    /// The root made the process's for good, at an address that never changes: for an entry
    /// point that still demands `Alloc<'static>` or `SharedAlloc<'static>` (the list in the
    /// lane's report; each is to take a lifetime instead). Its value is moved into its own
    /// first carved block, and its pages are never given back: deliberately, as the name says,
    /// and counted by the heap ratchet's `leak` column (scripts/src/bin/ambient/heap.rs) so it
    /// can be driven to zero. Written `Root::leak(root)`, never as a method, so the ratchet reads
    /// every call. Errs when the reservation cannot hold the root itself.
    pub fn leak(this: Root) -> Result<&'static Root, MemError> {
        let refused = |why: Refused, root: &Root| why.error(root.region.tag(), root.region.reserved());
        let class = class_for(Layout::new::<Root>()).map_err(|why| refused(why, &this))?;
        let at = this.carve(class).map_err(|why| refused(why, &this))?;
        let place = this.region.at(at).cast::<Root>();
        // SAFETY: `[at, at + class_size(class))` was just carved from this root's region, so it
        // is committed (the carve's `ensure`), inside the reservation and reached by no other
        // code; it is at least `size_of::<Root>()` long and aligned to `class_align(class)`, at
        // least `align_of::<Root>()` (`class_for`). `this` is moved into it whole.
        unsafe { place.write(this) };
        // SAFETY: the place holds the root just written. Nothing can take it out or drop it (the
        // one path to it is this shared reference), so the region it owns, whose pages hold it,
        // is never released: the reference is valid for the rest of the process, and the value
        // never moves, as a handle's allocator must not (blaze-own's invariant 3).
        Ok(unsafe { place.as_ref() })
    }

    /// What it holds now.
    pub fn used(&self) -> RootUsed {
        let mut live_blocks: isize = 0;
        let mut live_bytes: isize = 0;
        let mut allocations: usize = 0;
        for shard in &self.shards {
            live_blocks = live_blocks.wrapping_add(shard.live_blocks.load(Ordering::Acquire));
            live_bytes = live_bytes.wrapping_add(shard.live_bytes.load(Ordering::Acquire));
            allocations = allocations.wrapping_add(shard.allocations.load(Ordering::Acquire));
        }
        RootUsed {
            live_blocks: usize::try_from(live_blocks).unwrap_or(0),
            live_bytes: usize::try_from(live_bytes).unwrap_or(0),
            allocations,
            carved: self.carved.0.load(Ordering::Acquire),
            committed: self.region.committed(),
            reserved: self.region.reserved(),
        }
    }

    /// The tag its pages count to.
    pub fn tag(&self) -> Tag {
        self.region.tag()
    }

    /// Checks [`LiveAtDrop::Refused`] when the root ends or resets.
    fn check_live_at_end(&mut self, doing: &str) {
        let mut live: isize = 0;
        let mut bytes: isize = 0;
        for shard in self.shards.iter_mut() {
            live = live.wrapping_add(*shard.live_blocks.get_mut());
            bytes = bytes.wrapping_add(*shard.live_bytes.get_mut());
        }
        // A test's (debug) build: a block still live is a leak, the test's failure. Not while a
        // panic unwinds, which leaves blocks live by design and would abort on a second panic.
        if cfg!(debug_assertions) && self.live_at_drop == LiveAtDrop::Refused && live != 0 && !std::thread::panicking() {
            panic!(
                "blaze_mem::Root ({}, {} bytes reserved) {doing} with {live} blocks ({bytes} bytes) still live: their \
                 owners leaked them (`mem::forget`, `Vec::leak`, `Box::leak`, a cycle of counted pointers)",
                self.region.tag(),
                self.region.reserved()
            );
        }
    }

    /// Resets the root for reuse, returning every committed page to the kernel while keeping the
    /// virtual reservation mapped (avoiding an `munmap` + `mmap` pair between phases).
    ///
    /// Taking `&mut self` guarantees no [`Alloc`] or [`SharedAlloc`] handle borrowed from this
    /// root is alive. In debug builds with [`LiveAtDrop::Refused`], panics if any block was
    /// leaked, matching [`Drop`]. Returns the committed bytes given back.
    pub fn reset(&mut self) -> usize {
        self.check_live_at_end("reset");
        let committed = self.region.committed();
        *self.owner_shard.get_mut() = usize::MAX;
        *self.shared_mode.get_mut() = false;
        *self.carved.0.get_mut() = 0;
        *self.refusal.0.kind.get_mut() = Refused::NONE;
        *self.refusal.0.wanted.get_mut() = 0;
        *self.refusal.0.errno.get_mut() = 0;
        for list in self.lists.iter_mut() {
            *list.held.get_mut() = false;
            *list.top.get_mut() = 0;
        }
        for shard in self.shards.iter_mut() {
            *shard.live_blocks.get_mut() = 0;
            *shard.live_bytes.get_mut() = 0;
            *shard.allocations.get_mut() = 0;
            *shard.held.get_mut() = false;
            for count in shard.counts.iter_mut() {
                *count.get_mut() = 0;
            }
            for head in shard.heads.iter_mut() {
                *head.get_mut() = 0;
            }
        }
        if committed != 0 {
            // A refused decommit leaves the pages committed: memory held, nothing broken.
            let _ = self.region.decommit_after(0);
        }
        committed
    }

    /// Why its last take was refused ([`MemError::Exhausted`] naming the reservation, a commit
    /// the kernel refused, a size or alignment past what a root gives); `None` if none was.
    /// The allocator's error, `AllocError`, carries nothing, so this is where it is named.
    pub fn refusal(&self) -> Option<MemError> {
        let wanted = self.refusal.0.wanted.load(Ordering::Acquire);
        let why = match self.refusal.0.kind.load(Ordering::Acquire) {
            Refused::EXHAUSTED => Refused::Exhausted { wanted },
            Refused::COMMIT => Refused::Commit { bytes: wanted, errno: self.refusal.0.errno.load(Ordering::Acquire) },
            Refused::TOO_LARGE => Refused::TooLarge { wanted },
            Refused::ALIGN => Refused::Align { wanted },
            _ => return None,
        };
        Some(why.error(self.region.tag(), self.region.reserved()))
    }

    /// Where `ptr` is within the reservation, if it is in it: the same on every run of the same
    /// calls on one thread (see the module).
    pub fn offset_of(&self, ptr: NonNull<u8>) -> Option<usize> {
        let at = ptr.addr().get().wrapping_sub(self.region.base().addr().get());
        (at < self.region.reserved()).then_some(at)
    }

    /// Keeps `why` for [`Root::refusal`]; the allocator's error.
    #[cold]
    fn refuse(&self, why: Refused) -> AllocError {
        let (kind, wanted, errno) = match why {
            Refused::Exhausted { wanted } => (Refused::EXHAUSTED, wanted, 0),
            Refused::Commit { bytes, errno } => (Refused::COMMIT, bytes, errno),
            Refused::TooLarge { wanted } => (Refused::TOO_LARGE, wanted, 0),
            Refused::Align { wanted } => (Refused::ALIGN, wanted, 0),
        };
        self.refusal.0.wanted.store(wanted, Ordering::Release);
        self.refusal.0.errno.store(errno, Ordering::Release);
        self.refusal.0.kind.store(kind, Ordering::Release);
        AllocError
    }

    /// The offset of a block this root gave; anything else stops the process, naming it (a
    /// block freed into the wrong allocator).
    #[track_caller]
    fn block_at(&self, ptr: NonNull<u8>) -> usize {
        match self.offset_of(ptr) {
            Some(at) => at,
            None => panic!(
                "blaze_mem::Root ({}): {ptr:p} is not in its reservation of {} bytes at {:p}: a block given to the \
                 wrong allocator",
                self.region.tag(),
                self.region.reserved(),
                self.region.base()
            ),
        }
    }

    /// The class of a block this root gave with `layout` (which `class_for` took then).
    #[track_caller]
    fn class_of_block(&self, layout: Layout) -> usize {
        match class_for(layout) {
            Ok(class) => class,
            Err(why) => panic!(
                "blaze_mem::Root ({}): a block of {} bytes aligned to {} given back, a layout it never grants ({why:?}): \
                 a block given to the wrong allocator",
                self.region.tag(),
                layout.size(),
                layout.align()
            ),
        }
    }

    #[inline]
    fn shard(&self, idx: usize) -> &Shard {
        self.shards.get(idx & (SHARDS - 1)).unwrap_or(&self.shards[0])
    }

    /// Tracks whether more than one thread shard has accessed this root; switches small-class
    /// carving from exact 1-block mode to batch carving once shared across threads.
    #[inline]
    fn is_shared(&self, shard_idx: usize) -> bool {
        if self.shared_mode.load(Ordering::Relaxed) {
            return true;
        }
        let owner = self.owner_shard.load(Ordering::Relaxed);
        if owner == shard_idx {
            return false;
        }
        if owner == usize::MAX
            && self
                .owner_shard
                .compare_exchange(usize::MAX, shard_idx, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            return false;
        }
        self.shared_mode.store(true, Ordering::Relaxed);
        true
    }

    /// The next block of `class` from the reservation's end.
    fn carve(&self, class: usize) -> Result<usize, Refused> {
        self.carve_batch(class, 1).map(|(start, _)| start)
    }

    /// Carves up to `want` contiguous blocks of `class` in one `carved` CAS + one `ensure`.
    fn carve_batch(&self, class: usize, want: usize) -> Result<(usize, usize), Refused> {
        let (size, align) = (class_size(class), class_align(class));
        let reserved = self.region.reserved();
        // Acquire: a block uncarved by another thread was given back after its last use there.
        let mut from = self.carved.0.load(Ordering::Acquire);
        // Lock-free: each turn that fails saw another thread move the end.
        loop {
            let start = crate::align::checked_align_up(from, align).ok_or(Refused::Exhausted { wanted: size })?;
            let avail = reserved.checked_sub(start).ok_or(Refused::Exhausted { wanted: size })?;
            let fit = avail / size;
            if fit == 0 {
                return Err(Refused::Exhausted { wanted: size });
            }
            let count = want.min(fit).max(1);
            let end = start + count * size;
            // Committed before the end is published, so every block is backed once it is anyone's.
            self.region.ensure(end).map_err(|error| Refused::of_mem(&error, end))?;
            match self.carved.0.compare_exchange_weak(from, end, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok((start, count)),
                Err(now) => from = now,
            }
        }
    }

    /// The top free block of `class`, now the caller's.
    fn pop(&self, class: usize) -> Option<usize> {
        let list = self.lists.get(class)?;
        if list.top.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let _held = list.lock(class, self.region.tag());
        let top = list.top.load(Ordering::Relaxed);
        let at = top.checked_sub(1)?;
        let link = self.region.at(at).cast::<usize>();
        // SAFETY: `at` is a free block of this class (only `push` puts it on the list): carved,
        // so committed and inside the reservation, at least `GRAIN` bytes and aligned to `GRAIN`
        // (so to `usize`); its first word was written by `push` under this list's lock, which is
        // held here (so that write happened before), and no owner reaches a free block.
        let next = unsafe { link.read() };
        list.top.store(next, Ordering::Relaxed);
        Some(at)
    }

    /// Pops up to `max` blocks from the shared `class` list in one lock acquisition: returns
    /// `(first_at, rest_head_top, rest_tail_at, rest_count)`.
    fn pop_batch(&self, class: usize, max: usize) -> Option<(usize, usize, usize, u8)> {
        let list = self.lists.get(class)?;
        if list.top.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let _held = list.lock(class, self.region.tag());
        let first_top = list.top.load(Ordering::Relaxed);
        let first_at = first_top.checked_sub(1)?;
        // SAFETY: `first_at` is on this class's free list under `list.lock`.
        let mut next_top = unsafe { self.region.at(first_at).cast::<usize>().read() };
        if max <= 1 || next_top == 0 {
            list.top.store(next_top, Ordering::Relaxed);
            return Some((first_at, 0, 0, 0));
        }
        let rest_head_top = next_top;
        let mut rest_tail_at = 0usize;
        let mut rest_count = 0u8;
        let rest_limit = u8::try_from(max - 1).unwrap_or(LOCAL_FLUSH).min(LOCAL_FLUSH);
        while rest_count < rest_limit && next_top != 0 {
            let Some(at) = next_top.checked_sub(1) else { break };
            // SAFETY: `at` is a free block on this class's list under `list.lock`.
            next_top = unsafe { self.region.at(at).cast::<usize>().read() };
            rest_tail_at = at;
            rest_count += 1;
        }
        list.top.store(next_top, Ordering::Relaxed);
        if rest_count > 0 {
            // SAFETY: `rest_tail_at` was just detached from `list` and belongs to this caller.
            unsafe { self.region.at(rest_tail_at).cast::<usize>().write(0) };
        }
        Some((first_at, rest_head_top, rest_tail_at, rest_count))
    }

    /// Puts the block at `at` (of `class`, its owner done with it) on the class's list.
    fn push(&self, class: usize, at: usize) {
        let Some(list) = self.lists.get(class) else { return };
        let _held = list.lock(class, self.region.tag());
        let next = list.top.load(Ordering::Relaxed);
        let link = self.region.at(at).cast::<usize>();
        // SAFETY: the block is this root's, of this class (carved: committed, inside the
        // reservation, at least `GRAIN` bytes, aligned to `GRAIN`); its owner gave it back, so
        // nothing else reaches it; the list's lock is held, so no `pop` reads it meanwhile.
        unsafe { link.write(next) };
        list.top.store(at + 1, Ordering::Relaxed);
    }

    /// Splices an intrusive chain `[head_top ..= tail_at]` onto the shared `class` list in O(1).
    fn push_chain(&self, class: usize, head_top: usize, tail_at: usize) {
        let Some(list) = self.lists.get(class) else { return };
        let _held = list.lock(class, self.region.tag());
        let old_top = list.top.load(Ordering::Relaxed);
        // SAFETY: `tail_at` is the last block of a detached free chain of `class` owned by this
        // thread; under `list.lock` no reader reaches it until `list.top` publishes `head_top`.
        unsafe { self.region.at(tail_at).cast::<usize>().write(old_top) };
        list.top.store(head_top, Ordering::Relaxed);
    }

    /// Pops one free block of small `class` from `shard`'s local magazine.
    #[inline]
    fn pop_local(&self, shard: &Shard, class: usize) -> Option<usize> {
        let head_slot = shard.heads.get(class)?;
        if head_slot.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let count_slot = shard.counts.get(class)?;
        let _held = shard.lock(class, self.region.tag());
        let top = head_slot.load(Ordering::Relaxed);
        let at = top.checked_sub(1)?;
        // SAFETY: `at` is a free block of `class` in `shard`'s magazine, protected by `shard.lock`.
        let next = unsafe { self.region.at(at).cast::<usize>().read() };
        head_slot.store(next, Ordering::Relaxed);
        count_slot.store(count_slot.load(Ordering::Relaxed).saturating_sub(1), Ordering::Relaxed);
        Some(at)
    }

    /// Prepends a detached chain `[head_top ..= tail_at]` of `count` blocks onto `shard`'s local
    /// magazine for small `class`.
    fn splice_local(&self, shard: &Shard, class: usize, head_top: usize, tail_at: usize, count: u8) {
        let (Some(head_slot), Some(count_slot)) = (shard.heads.get(class), shard.counts.get(class)) else {
            return;
        };
        let _held = shard.lock(class, self.region.tag());
        let old_head = head_slot.load(Ordering::Relaxed);
        // SAFETY: `tail_at` is the tail of a newly carved or detached free chain owned by this
        // thread; joining it to `old_head` under `shard.lock` publishes it safely.
        unsafe { self.region.at(tail_at).cast::<usize>().write(old_head) };
        head_slot.store(head_top, Ordering::Relaxed);
        count_slot.store(count_slot.load(Ordering::Relaxed).saturating_add(count), Ordering::Relaxed);
    }

    /// Pushes `at` onto `shard`'s local magazine for small `class`, spilling [`LOCAL_FLUSH`] older
    /// blocks to the shared list in one O(1) splice when the magazine reaches [`LOCAL_HIGH_WATER`].
    fn push_local(&self, shard: &Shard, class: usize, at: usize) {
        let (Some(head_slot), Some(count_slot)) = (shard.heads.get(class), shard.counts.get(class)) else {
            self.push(class, at);
            return;
        };
        let spill = {
            let _held = shard.lock(class, self.region.tag());
            let next = head_slot.load(Ordering::Relaxed);
            // SAFETY: `at` is a returned block of `class`, written under `shard.lock`.
            unsafe { self.region.at(at).cast::<usize>().write(next) };
            head_slot.store(at + 1, Ordering::Relaxed);
            let count = count_slot.load(Ordering::Relaxed).saturating_add(1);
            if count >= LOCAL_HIGH_WATER {
                // Keep `at` (hottest in L1) at the magazine head and detach `LOCAL_FLUSH` older
                // blocks immediately behind it for a single batch push to the shared list.
                let flush_head_top = next;
                let mut cur_top = next;
                let mut flush_tail_at = 0usize;
                let mut flushed = 0u8;
                while flushed < LOCAL_FLUSH && cur_top != 0 {
                    let Some(node_at) = cur_top.checked_sub(1) else { break };
                    // SAFETY: `node_at` is a free block on this shard's list under `shard.lock`.
                    cur_top = unsafe { self.region.at(node_at).cast::<usize>().read() };
                    flush_tail_at = node_at;
                    flushed += 1;
                }
                if flushed > 0 {
                    // SAFETY: `at` is still held under `shard.lock`.
                    unsafe { self.region.at(at).cast::<usize>().write(cur_top) };
                    count_slot.store(count - flushed, Ordering::Relaxed);
                    Some((flush_head_top, flush_tail_at))
                } else {
                    count_slot.store(count, Ordering::Relaxed);
                    None
                }
            } else {
                count_slot.store(count, Ordering::Relaxed);
                None
            }
        };
        if let Some((flush_head_top, flush_tail_at)) = spill {
            self.push_chain(class, flush_head_top, flush_tail_at);
        }
    }

    /// Cold exhaustion fallback: reclaims one cached block of small `class` from sibling shards
    /// before reporting `Exhausted`.
    #[cold]
    fn steal_from_other_shards(&self, home: usize, class: usize) -> Option<usize> {
        for step in 1..SHARDS {
            let idx = (home + step) & (SHARDS - 1);
            let Some(other) = self.shards.get(idx) else { continue };
            if let Some(at) = self.pop_local(other, class) {
                return Some(at);
            }
        }
        None
    }

    /// Allocates one block of small `class` (`< LOCAL_CLASSES`) via `shard`'s local magazine,
    /// batch-refilling from the shared list or batch-carving from `carved` on a miss.
    fn alloc_small(&self, shard_idx: usize, shard: &Shard, class: usize, shared: bool) -> Result<usize, Refused> {
        if let Some(at) = self.pop_local(shard, class) {
            return Ok(at);
        }
        let want = if shared { batch_size(class) } else { 1 };
        if let Some((first_at, rest_head, rest_tail, rest_count)) = self.pop_batch(class, want) {
            if rest_count > 0 {
                self.splice_local(shard, class, rest_head, rest_tail, rest_count);
            }
            return Ok(first_at);
        }
        match self.carve_batch(class, want) {
            Ok((start, count)) => {
                if count > 1 {
                    let size = class_size(class);
                    let extra = count - 1;
                    // Link freshly carved blocks `1..count` into a chain outside any lock.
                    for i in 1..count {
                        let block_at = start + i * size;
                        let next_top = if i + 1 < count { start + (i + 1) * size + 1 } else { 0 };
                        // SAFETY: `[start, start + count * size)` was just carved and committed
                        // for this thread alone; `size >= GRAIN >= align_of::<usize>()`.
                        unsafe { self.region.at(block_at).cast::<usize>().write(next_top) };
                    }
                    let head_top = start + size + 1;
                    let tail_at = start + extra * size;
                    let extra_u8 = u8::try_from(extra).unwrap_or(u8::MAX);
                    self.splice_local(shard, class, head_top, tail_at, extra_u8);
                }
                Ok(start)
            }
            Err(why @ Refused::Exhausted { .. }) => {
                if let Some(at) = self.steal_from_other_shards(shard_idx, class) {
                    Ok(at)
                } else {
                    Err(why)
                }
            }
            Err(why) => Err(why),
        }
    }

    /// Moves the carved part's end from the end of the block at `at` (`old` bytes long) to
    /// `new` bytes after `at`, if that block ends it and the reservation has the room: the block
    /// resized in place. `false` when it is not the last block or there is no room.
    fn resize_last(&self, at: usize, old: usize, new: usize) -> bool {
        let (old_end, Some(new_end)) = (at + old, at.checked_add(new)) else { return false };
        if new_end > self.region.reserved() || self.carved.0.load(Ordering::Acquire) != old_end {
            return false;
        }
        if self.region.ensure(new_end).is_err() {
            // Not refused here: the move that follows takes its own block, and names a refusal.
            return false;
        }
        self.carved.0.compare_exchange(old_end, new_end, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    }

    /// [`Allocator::grow`] and [`Allocator::shrink`]: in place when the class stays or the
    /// block is the last carved, else moved, its first `kept` bytes copied.
    ///
    /// # Safety
    ///
    /// The caller's contract for `grow` or `shrink`.
    unsafe fn resize(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let from = self.class_of_block(old);
        let to = class_for(new).map_err(|why| self.refuse(why))?;
        let at = self.block_at(ptr);
        let stays = to == from
            || (crate::align::is_aligned(at, class_align(to)) && self.resize_last(at, class_size(from), class_size(to)));
        if stays {
            let shard = self.shard(thread_shard());
            if new.size() >= old.size() {
                shard
                    .live_bytes
                    .fetch_add(isize::try_from(new.size() - old.size()).unwrap_or(0), Ordering::Relaxed);
            } else {
                shard
                    .live_bytes
                    .fetch_sub(isize::try_from(old.size() - new.size()).unwrap_or(0), Ordering::Relaxed);
            }
            return Ok(NonNull::slice_from_raw_parts(ptr, new.size()));
        }
        let moved = self.allocate(new)?;
        let kept = old.size().min(new.size());
        // SAFETY: `ptr` is live for `old.size()` bytes (the caller's contract) and `moved` was
        // just given for `new.size()`; `kept` is at most both, and the blocks are distinct (one
        // live, one just taken).
        unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), moved.cast::<u8>().as_ptr(), kept) };
        // SAFETY: `ptr` is a live block of this root made with `old` (the caller's contract),
        // its bytes now copied, and no longer used.
        unsafe { self.deallocate(ptr, old) };
        Ok(moved)
    }
}

// SAFETY: a block given is `[at, at + class_size)` of this root's reservation, carved (committed
// before the end that includes it was published) or popped from its class's free list, and so
// held by no other owner: carves are disjoint (the end only moves by compare-and-swap from the
// value the carve read), a free block is on one list once (pushed by its owner's give, popped
// under the list's lock), and an uncarved block was the last one carved, given back. It is at
// least the layout's size (its class's) and aligned to the layout's alignment (`class_for` takes
// a class whose alignment is at least it; a carve aligns to the class's; a resize in place keeps
// the offset only when it is aligned to the new class's; the base is aligned to `MAX_ALIGN`). A
// give or resize of a block names its layout, from which its class is found again, so it goes
// back on the list it came for. The memory stays mapped until the root drops, which no handle
// outlives (they borrow it). Each method keeps the caller's part of its contract.
unsafe impl Allocator for Root {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let class = class_for(layout).map_err(|why| self.refuse(why))?;
        let shard_idx = thread_shard();
        let shard = self.shard(shard_idx);
        let shared = self.is_shared(shard_idx);
        let at = if class < LOCAL_CLASSES {
            self.alloc_small(shard_idx, shard, class, shared).map_err(|why| self.refuse(why))?
        } else {
            match self.pop(class) {
                Some(at) => at,
                None => self.carve(class).map_err(|why| self.refuse(why))?,
            }
        };
        shard.live_blocks.fetch_add(1, Ordering::Relaxed);
        shard.live_bytes.fetch_add(isize::try_from(layout.size()).unwrap_or(0), Ordering::Relaxed);
        shard.allocations.fetch_add(1, Ordering::Relaxed);
        Ok(NonNull::slice_from_raw_parts(self.region.at(at), layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let class = self.class_of_block(layout);
        let at = self.block_at(ptr);
        let shard_idx = thread_shard();
        let shard = self.shard(shard_idx);
        shard.live_blocks.fetch_sub(1, Ordering::Relaxed);
        shard.live_bytes.fetch_sub(isize::try_from(layout.size()).unwrap_or(0), Ordering::Relaxed);
        let shared = self.is_shared(shard_idx);
        // Uncarve the last block when single-threaded (preserves exact stack-like uncarving and
        // in-place Vec growth) or for large classes; in multi-threaded small-class mode skip the
        // global `carved` CAS and return directly to the thread's own shard magazine.
        if !shared || class >= LOCAL_CLASSES {
            let end = at + class_size(class);
            if self.carved.0.compare_exchange(end, at, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                return;
            }
        }
        if class < LOCAL_CLASSES {
            self.push_local(shard, class, at);
        } else {
            self.push(class, at);
        }
    }

    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract for `grow`, passed on.
        unsafe { self.resize(ptr, old, new) }
    }

    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract for `shrink`, passed on.
        unsafe { self.resize(ptr, old, new) }
    }
}

// SAFETY: `#[repr(C)]` with the header its first field, made by `Header::of::<Root>()` in
// `Root::new`, the only constructor, and never written after (the field is private).
unsafe impl Headed for Root {}

impl Drop for Root {
    fn drop(&mut self) {
        self.check_live_at_end("dropped");
        // The region, dropped after this, gives every page back to the kernel.
    }
}

impl fmt::Debug for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Root").field("tag", &self.region.tag()).field("used", &self.used()).finish()
    }
}

/// Thread-safe bump-style arena backed by a [`Root`] with [`LiveAtDrop::Freed`].
/// Heap-allocate in a [`blaze_own::Box`] before calling [`blaze_own::Bump::of`] so its
/// [`Header`] stays at a fixed address and never occupies a worker stack.
#[repr(C)]
pub struct SharedBump {
    header: Header,
    root: Root,
}

impl SharedBump {
    pub fn new(reserve: usize, tag: Tag) -> Result<Self, MemError> {
        let root = Root::new(RootOpts { reserve, tag, live_at_drop: LiveAtDrop::Freed })?;
        Ok(Self { header: Header::of::<SharedBump>(), root })
    }
}

// SAFETY: every method delegates directly to `Root`'s `Allocator` implementation.
unsafe impl Allocator for SharedBump {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.root.allocate(layout)
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.root.allocate_zeroed(layout)
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: caller's contract, forwarded to `self.root`.
        unsafe { self.root.deallocate(ptr, layout) }
    }

    #[inline]
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: caller's contract, forwarded to `self.root`.
        unsafe { self.root.grow(ptr, old, new) }
    }

    #[inline]
    unsafe fn grow_zeroed(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: caller's contract, forwarded to `self.root`.
        unsafe { self.root.grow_zeroed(ptr, old, new) }
    }

    #[inline]
    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: caller's contract, forwarded to `self.root`.
        unsafe { self.root.shrink(ptr, old, new) }
    }
}

// SAFETY: `#[repr(C)]` with `header: Header::of::<SharedBump>()` as its first field, immutable
// after construction.
unsafe impl Headed for SharedBump {}

// SAFETY: `self.root` is created with `LiveAtDrop::Freed`, so every frozen bump allocation is
// reclaimed in bulk when `SharedBump` drops.
unsafe impl blaze_own::BumpAlloc for SharedBump {}

#[cfg(test)]
mod magazine_tests {
    use core::alloc::{Allocator, Layout};
    use std::{sync::mpsc, thread};

    use super::{LiveAtDrop, Root, RootOpts, SHARDS, Shard, Tag};

    #[test]
    fn root_sharded_magazines_multithread_alloc_cross_free_and_drop() {
        assert_eq!(SHARDS, 128);
        assert_eq!(align_of::<Shard>(), 128);
        assert!(size_of::<Root>() <= 64 << 10);

        let root = Root::new(RootOpts::TEST).expect("test root");
        let shared = root.shared();
        const WORKERS: usize = 16;
        const ITERS: usize = 256;

        // 1. Concurrent small-class churn triggering batch carve, magazine flush (>=64), and refill.
        thread::scope(|scope| {
            for tid in 0..WORKERS {
                scope.spawn(move || {
                    let mut held: blaze_own::Vec<(core::ptr::NonNull<u8>, Layout, u8), _> =
                        blaze_own::Vec::with_capacity_in(96, shared);
                    for round in 0..4usize {
                        let sz = [16usize, 32, 64, 128, 256, 512, 1024][(tid + round) % 7];
                        let layout = Layout::from_size_align(sz, 16).expect("layout");
                        for i in 0..96usize {
                            let ptr = shared.allocate(layout).expect("alloc").cast::<u8>();
                            let fill = u8::try_from((tid.wrapping_add(i).wrapping_add(round)) & 0xff).unwrap_or(0);
                            // SAFETY: `ptr` is uniquely owned and `sz` bytes long.
                            unsafe { core::ptr::write_bytes(ptr.as_ptr(), fill, sz) };
                            held.push((ptr, layout, fill));
                        }
                        for (ptr, layout, fill) in held.drain(..) {
                            // SAFETY: `ptr` is still live with `layout.size()` initialized bytes.
                            let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), layout.size()) };
                            assert!(bytes.iter().all(|&b| b == fill), "data integrity on thread {tid}");
                            // SAFETY: `ptr` was allocated from `shared` with `layout`.
                            unsafe { shared.deallocate(ptr, layout) };
                        }
                    }
                    // Also churn short-lived allocations.
                    let small = Layout::from_size_align(48, 16).expect("layout");
                    for _ in 0..ITERS {
                        let ptr = shared.allocate(small).expect("alloc").cast::<u8>();
                        // SAFETY: allocated above with `small`.
                        unsafe { shared.deallocate(ptr, small) };
                    }
                });
            }
        });

        let mid = root.used();
        assert_eq!((mid.live_blocks, mid.live_bytes), (0, 0), "after churn: {mid:?}");
        assert!(mid.allocations >= WORKERS * (4 * 96 + ITERS));

        // 2. Cross-thread alloc -> free verifies signed per-shard deltas sum to exact zero.
        let (tx, rx) = mpsc::channel::<(usize, Layout)>();
        thread::scope(|scope| {
            let producer = scope.spawn(move || {
                let layout = Layout::from_size_align(64, 16).expect("layout");
                for i in 0..200u8 {
                    let ptr = shared.allocate(layout).expect("alloc").cast::<u8>();
                    // SAFETY: live 64-byte block owned by producer until sent.
                    unsafe { core::ptr::write_bytes(ptr.as_ptr(), i, 64) };
                    tx.send((ptr.addr().get(), layout)).expect("send");
                }
            });
            let consumer = scope.spawn(move || {
                for (expected, (addr, layout)) in rx.into_iter().enumerate() {
                    let ptr = core::ptr::NonNull::new(core::ptr::with_exposed_provenance_mut::<u8>(addr))
                        .expect("non-null");
                    let byte = u8::try_from(expected & 0xff).unwrap_or(0);
                    // SAFETY: received unique ownership from producer.
                    let slice = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), layout.size()) };
                    assert!(slice.iter().all(|&b| b == byte));
                    // SAFETY: allocated from `shared` with `layout`.
                    unsafe { shared.deallocate(ptr, layout) };
                }
            });
            producer.join().expect("producer");
            consumer.join().expect("consumer");
        });

        let end = root.used();
        assert_eq!((end.live_blocks, end.live_bytes), (0, 0), "after cross-thread free: {end:?}");
        drop(root);

        // 3. Dropping a second Root while threads are still alive succeeds with zero TLS races.
        let root2 = Root::new(RootOpts {
            reserve: 2 << 20,
            tag: Tag::Other,
            live_at_drop: LiveAtDrop::Refused,
        })
        .expect("root2");
        let l = Layout::from_size_align(64, 16).expect("layout");
        let p = root2.alloc().allocate(l).expect("alloc").cast::<u8>();
        // SAFETY: live block from `root2`.
        unsafe { root2.alloc().deallocate(p, l) };
        drop(root2);
    }
}
