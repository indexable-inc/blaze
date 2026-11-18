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
    fmt, hint,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering},
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

/// One class's free blocks.
struct List {
    /// Its lock (see the module).
    held: AtomicBool,
    /// The offset of the top free block plus one; 0 when there is none. Written only with
    /// `held`; read without it only as a hint that the list is empty.
    top: AtomicUsize,
}

impl List {
    const EMPTY: List = List { held: AtomicBool::new(false), top: AtomicUsize::new(0) };

    /// Takes the list's lock: bounded (see the module), else the process stops naming `class`.
    #[expect(
        clippy::disallowed_methods,
        reason = "the root heap is below blaze_graph and its spin-then-park helper; the section it guards is four memory operations, and the spin is bounded by YIELDS, past which it stops naming the class (rules 12 and 18)"
    )]
    fn lock(&self, class: usize, tag: Tag) -> Locked<'_> {
        let mut yields = 0u32;
        loop {
            // Acquire: what the last holder wrote of the list (a block's link) is seen here.
            if self.held.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return Locked { list: self };
            }
            let mut spins = 0u32;
            while self.held.load(Ordering::Relaxed) {
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
}

/// A held list lock, given back when dropped.
struct Locked<'l> {
    list: &'l List,
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // Release: what this holder wrote is seen by the next.
        self.list.held.store(false, Ordering::Release);
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

/// The root heap (see the module): made once at a program's or a test's top, handed down as
/// `&Root`, [`Alloc`] or [`SharedAlloc`].
///
/// `#[repr(C)]` with its [`Header`] first, so a handle is its address ([`Headed`]).
#[repr(C)]
pub struct Root {
    /// First: the handle's table (blaze-own's invariant 1).
    header: Header,
    region: Region,
    /// The end of the carved part, an offset into the reservation.
    carved: AtomicUsize,
    live_blocks: AtomicUsize,
    live_bytes: AtomicUsize,
    allocations: AtomicUsize,
    /// The last refusal, for [`Root::refusal`]: its kind ([`Refused`]'s codes), the size it
    /// named and the kernel's error number. Written by the refusing thread, read whole only by
    /// the caller it refused (another thread's refusal at once may mix the fields).
    refused: AtomicU8,
    refused_wanted: AtomicUsize,
    refused_errno: AtomicI32,
    lists: [List; CLASSES],
    /// The maker's choice for blocks live at the drop ([`RootOpts::live_at_drop`]).
    live_at_drop: LiveAtDrop,
}

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
            carved: AtomicUsize::new(0),
            live_blocks: AtomicUsize::new(0),
            live_bytes: AtomicUsize::new(0),
            allocations: AtomicUsize::new(0),
            refused: AtomicU8::new(Refused::NONE),
            refused_wanted: AtomicUsize::new(0),
            refused_errno: AtomicI32::new(0),
            lists: [const { List::EMPTY }; CLASSES],
            live_at_drop: opts.live_at_drop,
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
        RootUsed {
            live_blocks: self.live_blocks.load(Ordering::Acquire),
            live_bytes: self.live_bytes.load(Ordering::Acquire),
            allocations: self.allocations.load(Ordering::Acquire),
            carved: self.carved.load(Ordering::Acquire),
            committed: self.region.committed(),
            reserved: self.region.reserved(),
        }
    }

    /// The tag its pages count to.
    pub fn tag(&self) -> Tag {
        self.region.tag()
    }

    /// Why its last take was refused ([`MemError::Exhausted`] naming the reservation, a commit
    /// the kernel refused, a size or alignment past what a root gives); `None` if none was.
    /// The allocator's error, `AllocError`, carries nothing, so this is where it is named.
    pub fn refusal(&self) -> Option<MemError> {
        let wanted = self.refused_wanted.load(Ordering::Acquire);
        let why = match self.refused.load(Ordering::Acquire) {
            Refused::EXHAUSTED => Refused::Exhausted { wanted },
            Refused::COMMIT => Refused::Commit { bytes: wanted, errno: self.refused_errno.load(Ordering::Acquire) },
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
        self.refused_wanted.store(wanted, Ordering::Release);
        self.refused_errno.store(errno, Ordering::Release);
        self.refused.store(kind, Ordering::Release);
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

    /// The next block of `class` from the reservation's end.
    fn carve(&self, class: usize) -> Result<usize, Refused> {
        let (size, align) = (class_size(class), class_align(class));
        let reserved = self.region.reserved();
        // Acquire: a block uncarved by another thread was given back after its last use there.
        let mut from = self.carved.load(Ordering::Acquire);
        // Lock-free: each turn that fails saw another thread move the end.
        loop {
            let start = crate::align::checked_align_up(from, align).ok_or(Refused::Exhausted { wanted: size })?;
            let end = start.checked_add(size).filter(|&end| end <= reserved).ok_or(Refused::Exhausted { wanted: size })?;
            // Committed before the end is published, so a block is backed once it is anyone's.
            self.region.ensure(end).map_err(|error| Refused::of_mem(&error, end))?;
            match self.carved.compare_exchange_weak(from, end, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(start),
                Err(now) => from = now,
            }
        }
    }

    /// The top free block of `class`, now the caller's.
    fn pop(&self, class: usize) -> Option<usize> {
        let list = &self.lists[class];
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

    /// Puts the block at `at` (of `class`, its owner done with it) on the class's list.
    fn push(&self, class: usize, at: usize) {
        let list = &self.lists[class];
        let _held = list.lock(class, self.region.tag());
        let next = list.top.load(Ordering::Relaxed);
        let link = self.region.at(at).cast::<usize>();
        // SAFETY: the block is this root's, of this class (carved: committed, inside the
        // reservation, at least `GRAIN` bytes, aligned to `GRAIN`); its owner gave it back, so
        // nothing else reaches it; the list's lock is held, so no `pop` reads it meanwhile.
        unsafe { link.write(next) };
        list.top.store(at + 1, Ordering::Relaxed);
    }

    /// Moves the carved part's end from the end of the block at `at` (`old` bytes long) to
    /// `new` bytes after `at`, if that block ends it and the reservation has the room: the block
    /// resized in place. `false` when it is not the last block or there is no room.
    fn resize_last(&self, at: usize, old: usize, new: usize) -> bool {
        let (old_end, Some(new_end)) = (at + old, at.checked_add(new)) else { return false };
        if new_end > self.region.reserved() || self.carved.load(Ordering::Acquire) != old_end {
            return false;
        }
        if self.region.ensure(new_end).is_err() {
            // Not refused here: the move that follows takes its own block, and names a refusal.
            return false;
        }
        self.carved.compare_exchange(old_end, new_end, Ordering::AcqRel, Ordering::Relaxed).is_ok()
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
            if new.size() >= old.size() {
                self.live_bytes.fetch_add(new.size() - old.size(), Ordering::Relaxed);
            } else {
                self.live_bytes.fetch_sub(old.size() - new.size(), Ordering::Relaxed);
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
        let at = match self.pop(class) {
            Some(at) => at,
            None => self.carve(class).map_err(|why| self.refuse(why))?,
        };
        self.live_blocks.fetch_add(1, Ordering::Relaxed);
        self.live_bytes.fetch_add(layout.size(), Ordering::Relaxed);
        self.allocations.fetch_add(1, Ordering::Relaxed);
        Ok(NonNull::slice_from_raw_parts(self.region.at(at), layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let class = self.class_of_block(layout);
        let at = self.block_at(ptr);
        self.live_blocks.fetch_sub(1, Ordering::Relaxed);
        self.live_bytes.fetch_sub(layout.size(), Ordering::Relaxed);
        // The last block carved is uncarved; any other goes on its class's list. Release: its
        // owner's last writes come before the thread that carves those bytes again.
        let end = at + class_size(class);
        if self.carved.compare_exchange(end, at, Ordering::AcqRel, Ordering::Relaxed).is_err() {
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
        let live = *self.live_blocks.get_mut();
        // A test's (debug) build: a block still live is a leak, the test's failure. Not while a
        // panic unwinds, which leaves blocks live by design and would abort on a second panic.
        if cfg!(debug_assertions) && self.live_at_drop == LiveAtDrop::Refused && live != 0 && !std::thread::panicking() {
            let bytes = *self.live_bytes.get_mut();
            panic!(
                "blaze_mem::Root ({}, {} bytes reserved) dropped with {live} blocks ({bytes} bytes) still live: their \
                 owners leaked them (`mem::forget`, `Vec::leak`, `Box::leak`, a cycle of counted pointers)",
                self.region.tag(),
                self.region.reserved()
            );
        }
        // The region, dropped after this, gives every page back to the kernel.
    }
}

impl fmt::Debug for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Root").field("tag", &self.region.tag()).field("used", &self.used()).finish()
    }
}
