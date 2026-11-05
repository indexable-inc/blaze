//! Blocks in size classes, without the global allocator: for tables made and dropped whole
//! that grow by a new table twice the last (a solver cache's chunks, shards and probe tables:
//! [`take`], [`Slice`]), and for values that outlive every request and die one by one (a
//! parsed tree, a file's bytes, a token tree kept as a query's result: [`Kept`], the allocator
//! of their lifetime).
//!
//! A size class reserves its own region the first time a block of it is taken
//! ([`CLASS_BYTES`] of address space, nothing committed) and carves it in order; a block given
//! back goes on its class's lock-free stack and is the next one taken there, already backed by
//! pages. Pages go back to the kernel under a bound, the reservation and the commit kept, so
//! the fast path stays one compare-and-swap ([`Region::purge`], [`Region::reuse`]):
//!
//! - A block of at least [`AT_ONCE`] gives its pages back as it is given, onto the class's
//!   cold stack.
//! - A smaller block stays warm, pages and all, until the class's warm bytes pass [`KEEP`].
//!   Then the giver trims: it takes warm blocks off in steps of [`TRIM_STEP`], down to half of
//!   [`KEEP`] or [`TRIM_FUEL`] steps, sorts them all, gives back every page that runs of
//!   adjacent blocks cover whole, and moves those blocks to the cold stack; blocks that cover
//!   no whole page stay warm, and the next trim waits for another half [`KEEP`] of frees.
//! - A take pops a warm block first, then a cold one (made the process's again first: macOS
//!   counts its pages back, Windows commits them), then carves.
//!
//! What each class holds, has given back and keeps shows in [`snapshot`]; the counts a block
//! moves between are gathered per thread ([`GATHER`]), in the thread-local that gathers its
//! tags' counts (`tag::Gathered`), and folded into the class's at a step, so a kept value's
//! warm take or give writes no shared line but the stack's and its tag's live bytes, and
//! reaches its thread-local once.
//!
//! The classes are 64 B and up, [`STEPS`] to each doubling (64, 80, 96, 112, 128, 160, ...:
//! a block wastes at most a fifth of itself, about a tenth on average, where power-of-two
//! classes waste up to half and about 30% on average), one class of each power of two among
//! them. On a 32-bit target (wasm32), where a reservation is memory taken whole, the classes
//! are the powers of two alone, so fewer of them are made.
//!
//! A block is aligned to the largest power of two that divides its class's size, up to a page:
//! a class's region is page-aligned and its blocks are multiples of the size apart. A take
//! asking for more alignment than a class has gets the next class that has it (a power of two
//! has all it asks for, up to a page). A newly carved block is zero, as the kernel makes fresh
//! pages; a recycled one holds what its last owner left, and is cleared only for a caller that
//! asks ([`Fill::Zeroed`]).
//!
//! Cost: a warm take or give is one compare-and-swap on the class's stack and a thread-local
//! count; carving is one `fetch_add`, plus a commit system call every quarter of what the class
//! has committed; a large block's give is a system call, and a cold take faults its pages in.

use std::{fmt, marker::PhantomData, mem, ops::Deref, ptr::NonNull, sync::OnceLock};

// The class's counts are `std`'s atomics even under `cfg(blaze_check)`, as tag's lines are:
// process-wide counters every test shares, which a model execution would reset.
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

use crate::{
    MemError, Region, Tag,
    stack::{LinkTable, Links, NONE, Stack},
    sync::atomic::{AtomicU32, Ordering},
    tag,
};

/// The address space a size class reserves: 16 GB, free on a 64-bit machine even for every
/// class at once (about 1.8 TB of the 128 TB user space; only the classes used are made).
#[cfg(target_pointer_width = "64")]
pub const CLASS_BYTES: usize = 16 << 30;

/// On a 32-bit target (wasm32) a reservation is memory taken whole, so a class reserves 8 MB,
/// and only the classes used are made.
#[cfg(not(target_pointer_width = "64"))]
pub const CLASS_BYTES: usize = 8 << 20;

/// Classes to each doubling of size, as a power of two: four on a 64-bit target.
#[cfg(target_pointer_width = "64")]
const STEP_BITS: u32 = 2;

/// On a 32-bit target, where each class made costs its reservation whole, the powers of two
/// alone.
#[cfg(not(target_pointer_width = "64"))]
const STEP_BITS: u32 = 0;

/// Classes to each doubling of size.
pub const STEPS: usize = 1 << STEP_BITS;

/// The smallest block: a cache line.
pub const SMALLEST: usize = 64;

/// The largest block: a class of one.
pub const LARGEST: usize = CLASS_BYTES;

/// The largest alignment a block has for certain: the smallest page of the machines blaze
/// targets.
pub const MAX_ALIGN: usize = 4096;

/// Blocks at least this large give their pages back as they are given: a block of 64 pages or
/// more, whose owner wrote far more than the fault of each page costs to take it again.
pub const AT_ONCE: usize = 256 << 10;

/// The warm bytes a class of smaller blocks keeps, pages and all, before a giver trims it
/// down to half of this: about 2 MiB a class, under 0.2 GiB for every class blaze uses.
pub const KEEP: usize = 2 << 20;

/// The net bytes a thread gives or takes of one class before it adds them to the class's
/// counts: a class's count is short of the truth by at most this per thread.
pub const GATHER: usize = 64 << 10;

/// Blocks one step of a trim takes off the warm stack with one compare-and-swap.
pub const TRIM_STEP: usize = 512;

/// The steps of one trim at most: its bound (`bounded-progress`), 32,768 blocks, whose
/// indices it sorts together in the class's scratch ([`TRIM_MOST`]).
pub const TRIM_FUEL: usize = 64;

/// The blocks one trim takes at most.
pub const TRIM_MOST: usize = TRIM_STEP * TRIM_FUEL;

const MIN_SHIFT: u32 = SMALLEST.trailing_zeros();
const MAX_SHIFT: u32 = LARGEST.trailing_zeros();
const CLASSES: usize = (MAX_SHIFT - MIN_SHIFT) as usize * STEPS + 1;

const _: () = assert!(SMALLEST.is_power_of_two() && LARGEST.is_power_of_two());
const _: () = assert!(MIN_SHIFT >= STEP_BITS);
const _: () = assert!((CLASS_BYTES >> MIN_SHIFT) < crate::stack::NONE as usize);

/// The size of class `index`: `STEPS + step` times `2^(shift - STEP_BITS)` for the class's
/// doubling `shift` and its step within it.
const fn class_size(index: usize) -> usize {
    let shift = MIN_SHIFT + (index / STEPS) as u32;
    let step = index % STEPS;
    (STEPS + step) << (shift - STEP_BITS)
}

/// The alignment every block of class `index` has: the largest power of two dividing its size,
/// at most a page ([`MAX_ALIGN`]).
const fn class_align(index: usize) -> usize {
    let size = class_size(index);
    let align = size & size.wrapping_neg();
    if align > MAX_ALIGN { MAX_ALIGN } else { align }
}

/// What a taken block holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// Whatever it holds: a caller that writes every byte it reads.
    Any,
    /// Zeros.
    Zeroed,
}

struct Class {
    size: usize,
    /// The tag its committed bytes count to while no owner holds them: its free blocks, warm
    /// and cold, and what its region committed past its last carved block ([`Tag::Chunks`]; a
    /// test's own class, a tag of the test's). A block taken moves from it to its owner's tag,
    /// at the carve or at the class's fold of its free counts ([`Class::fold`]), so a block's
    /// bytes count once: to its owner while it lives, to this after.
    free: Tag,
    region: Region,
    links: LinkTable,
    max: u32,
    /// Its place among a thread's gathered counts ([`tag::Gathered`]): its index in [`CLASS`].
    /// `None` for a class made outside it (a test's own), which counts straight to its lines.
    slot: Option<usize>,
    /// The blocks a thread gathers before it folds them in: [`GATHER`] bytes, at least one.
    gather: u32,
    /// Whether a block given gives its pages back at once ([`AT_ONCE`]).
    at_once: bool,
    carved: AtomicU32,
    /// Blocks given back with their pages: committed, holding what their owners left.
    warm: Stack,
    /// Blocks given back whose whole pages went to the kernel ([`Region::purge`]), taken after
    /// the warm ones, each made the process's again first ([`Region::reuse`]).
    cold: Stack,
    counts: Counts,
    /// Where the one thread trimming ([`Counts::trimming`]) sorts the indices of the blocks it
    /// took: [`TRIM_MOST`] of them, committed when the class is made (128 KiB of address
    /// space a class; resident once the class has trimmed).
    scratch: Region,
}

/// A class's counts, in bytes; warm and cold are folded in from threads ([`GATHER`]), so each
/// may lag, or read below zero while one thread's gives wait to be folded and another's takes
/// are in.
struct Counts {
    warm: AtomicI64,
    cold: AtomicI64,
    /// Bytes of pages given back to the kernel, in all.
    given_back: AtomicU64,
    /// Trims done, in all.
    trims: AtomicU64,
    /// The warm bytes past which a giver trims: [`KEEP`], or half of it past what the last trim
    /// left when that is more (the blocks it could not give back): [`trim_at`].
    trim_at: AtomicI64,
    /// Held by the one thread trimming.
    trimming: AtomicBool,
}

impl Counts {
    const fn new() -> Counts {
        Counts {
            warm: AtomicI64::new(0),
            cold: AtomicI64::new(0),
            given_back: AtomicU64::new(0),
            trims: AtomicU64::new(0),
            trim_at: AtomicI64::new(KEEP as i64),
            trimming: AtomicBool::new(false),
        }
    }
}

/// The warm bytes past which a class trims when `left` are warm after a trim: [`KEEP`], or
/// half of it past `left`, so warm blocks a trim could not give back (they cover no whole
/// page) are walked again only after that much more is freed.
fn trim_at(left: i64) -> i64 {
    (left + (KEEP / 2) as i64).max(KEEP as i64)
}

/// Which free stack a count is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Free {
    Warm,
    Cold,
}

/// Folds the thread's gathered counts of every class in ([`tag::Gathered`]), trimming none:
/// at a [`snapshot`] and when the thread ends.
pub(crate) fn fold_thread(gathered: &tag::Gathered) {
    for slot in 0..CLASSES {
        let (warm, cold) = (gathered.warm[slot].replace(0), gathered.cold[slot].replace(0));
        if warm == 0 && cold == 0 {
            continue;
        }
        if let Some(Ok(class)) = CLASS[slot].get() {
            class.fold(Free::Warm, warm, false);
            class.fold(Free::Cold, cold, false);
        }
    }
}

/// What one trim did ([`Class::trim`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Trimmed {
    /// Bytes of pages given back to the kernel.
    given_back: usize,
    /// Another thread was trimming the class, so this one did nothing.
    busy: bool,
}

static CLASS: [OnceLock<Result<Class, String>>; CLASSES] = [const { OnceLock::new() }; CLASSES];

impl Class {
    /// A class of `size`-byte blocks over `space` bytes of address space, at `slot` of
    /// [`CLASS`] (`None` for one of its own), its committed bytes held by no owner counted to
    /// `free`.
    fn new(size: usize, space: usize, slot: Option<usize>, free: Tag) -> Result<Class, MemError> {
        let max = (space / size).max(1) as u32;
        Ok(Class {
            size,
            free,
            region: Region::reserve(max as usize * size, free)?,
            links: LinkTable::new(max, free)?,
            max,
            slot,
            gather: (GATHER / size).max(1) as u32,
            at_once: size >= AT_ONCE,
            carved: AtomicU32::new(0),
            warm: Stack::new(),
            cold: Stack::new(),
            counts: Counts::new(),
            scratch: {
                let scratch = Region::reserve(TRIM_MOST * size_of::<u32>(), free)?;
                scratch.ensure(TRIM_MOST * size_of::<u32>())?;
                scratch
            },
        })
    }

    fn size(&self) -> usize {
        self.size
    }

    fn take(&'static self, fill: Fill, tag: Tag) -> Result<Owned, MemError> {
        let (at, warm) = self.take_raw(fill)?;
        if warm {
            self.gather(Free::Warm, -1);
        }
        tag::add(tag, self.size());
        tag::allocated(tag, self.size());
        Ok(Owned { at, class: self, tag })
    }

    /// A block of the class, counted to no tag: the caller counts it, and when it came off the
    /// warm stack (`true`), counts it off the class's warm blocks ([`Class::gather`]) too, in
    /// the one thread-local access it makes for the tag.
    #[inline]
    fn take_raw(&self, fill: Fill) -> Result<(NonNull<u8>, bool), MemError> {
        let Some(index) = self.warm.pop(&self.links) else {
            return self.take_cold(fill).map(|at| (at, false));
        };
        let at = self.region.at(index as usize * self.size);
        if fill == Fill::Zeroed {
            // SAFETY: a block on the free stack was carved, so it is committed, and the pop
            // just now made it this caller's alone: no `Owned` names it.
            unsafe { at.write_bytes(0, self.size()) };
        }
        Ok((at, true))
    }

    /// A cold block made the process's again, else a block carved.
    #[inline(never)]
    fn take_cold(&self, fill: Fill) -> Result<NonNull<u8>, MemError> {
        let Some(index) = self.cold.pop(&self.links) else { return self.carve() };
        let offset = index as usize * self.size;
        if let Err(error) = self.region.reuse(offset, self.size) {
            self.cold.push(index, &self.links);
            return Err(error);
        }
        self.gather(Free::Cold, -1);
        let at = self.region.at(offset);
        if fill == Fill::Zeroed {
            // SAFETY: as in `take_raw`; its pages are the process's again (`reuse`), and what
            // was given back reads zeros or what was left, cleared here.
            unsafe { at.write_bytes(0, self.size()) };
        }
        Ok(at)
    }

    /// A block never handed out, committed, so zero.
    fn carve(&self) -> Result<NonNull<u8>, MemError> {
        let index = self.carved.fetch_add(1, Ordering::Relaxed);
        if index >= self.max {
            self.carved.fetch_min(self.max, Ordering::Relaxed);
            return Err(MemError::Exhausted {
                tag: Tag::Chunks,
                reserved: self.region.reserved(),
                wanted: (index as usize + 1) * self.size,
            });
        }
        let end = (index as usize + 1) * self.size;
        self.region.ensure(end)?;
        // SAFETY: `index` came from the `fetch_add` just now: no other thread has it.
        unsafe { self.links.init(index) }?;
        // Committed (counted to `free` by the region), now its taker's: the taker counts it.
        tag::sub(self.free, self.size);
        Ok(self.region.at(index as usize * self.size))
    }

    /// Gives the block at `at` back: onto the warm stack (`true`: the caller counts it there,
    /// [`Class::gather`]), or, for a class that gives back at once, its pages to the kernel and
    /// it onto the cold stack, counted.
    #[inline]
    fn give(&self, at: NonNull<u8>) -> bool {
        let offset = at.as_ptr() as usize - self.region.base().as_ptr() as usize;
        debug_assert_eq!(offset % self.size, 0, "a block's start");
        let index = (offset / self.size) as u32;
        if self.at_once {
            self.give_cold(index, offset);
            return false;
        }
        self.warm.push(index, &self.links);
        true
    }

    /// Gives a large block's pages back and puts it on the cold stack.
    #[inline(never)]
    fn give_cold(&self, index: u32, offset: usize) {
        // SAFETY: the giver gives the block up, so nothing reads it before writing it, and it
        // is touched again only by a take, which pops it off the cold stack and `reuse`s it
        // first; its pages are its own (`purge` rounds inward), no neighbor's.
        let given = unsafe { self.region.purge(offset, self.size) };
        self.counts.given_back.fetch_add(given as u64, Ordering::Relaxed);
        self.cold.push(index, &self.links);
        self.gather(Free::Cold, 1);
    }

    /// Counts `blocks` more (or fewer) on the `free` stack: on the thread until they reach the
    /// class's `gather`, then on the class's line.
    fn gather(&self, free: Free, blocks: i32) {
        let full =
            tag::gathered(|gathered| self.gather_in(gathered, free, blocks)).unwrap_or(blocks);
        if full != 0 {
            self.fold(free, full, true);
        }
    }

    /// Counts `blocks` more (or fewer) on the `free` stack in the thread's `gathered` counts:
    /// what to fold into the class's line now ([`Class::fold`]), 0 while gathered. A class
    /// made outside [`CLASS`] (a test's own) gathers nothing.
    #[inline]
    fn gather_in(&self, gathered: &tag::Gathered, free: Free, blocks: i32) -> i32 {
        let Some(slot) = self.slot else { return blocks };
        let cell = match free {
            Free::Warm => &gathered.warm[slot],
            Free::Cold => &gathered.cold[slot],
        };
        let now = cell.get() + blocks;
        if now.unsigned_abs() < self.gather {
            cell.set(now);
            return 0;
        }
        cell.set(0);
        now
    }

    /// Adds `blocks` to the class's count of the `free` stack, and their bytes to its free tag
    /// ([`Class::free`]: given, they are held by no owner; taken, by their taker, who counted
    /// them); past [`Counts::trim_at`] warm bytes, and when `may_trim`, trims. A trim moves
    /// blocks from warm to cold, held by no owner either way, so the free tag stays.
    ///
    /// Threads fold their gathered blocks up to [`GATHER`] a class late, so the free tag may
    /// read low (a block given on one thread, not folded yet, taken on another, folded) by that
    /// much a thread a class; its reads clamp at 0 ([`tag::snapshot`]).
    #[cold]
    #[inline(never)]
    fn fold(&self, free: Free, blocks: i32, may_trim: bool) {
        if blocks == 0 {
            return;
        }
        let bytes = i64::from(blocks) * self.size as i64;
        match usize::try_from(bytes) {
            Ok(given) => tag::add(self.free, given),
            Err(_) => tag::sub(self.free, bytes.unsigned_abs() as usize),
        }
        match free {
            Free::Cold => {
                self.counts.cold.fetch_add(bytes, Ordering::Relaxed);
            }
            Free::Warm => {
                let now = self.counts.warm.fetch_add(bytes, Ordering::Relaxed) + bytes;
                if bytes < 0 {
                    // Takes drew the warm blocks down: the next trim comes sooner.
                    self.counts.trim_at.fetch_min(trim_at(now), Ordering::Relaxed);
                } else if may_trim && now > self.counts.trim_at.load(Ordering::Relaxed) {
                    let trimmed = self.trim();
                    if !trimmed.busy {
                        self.counts.trims.fetch_add(1, Ordering::Relaxed);
                        self.counts
                            .given_back
                            .fetch_add(trimmed.given_back as u64, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Gives back the pages of warm blocks, down to half of [`KEEP`] warm bytes: takes blocks
    /// off the warm stack (the latest given, which are often neighbors: a tree's blocks die
    /// together) in steps of [`TRIM_STEP`], at most [`TRIM_FUEL`] of them, sorts them all,
    /// gives back every page that runs of adjacent ones cover whole, and moves those blocks to
    /// the cold stack. Blocks of a run that covers no whole page go back warm, and the next
    /// trim waits for half of [`KEEP`] past what is left ([`trim_at`]), so blocks that cannot
    /// go cost a trim once per half [`KEEP`] of frees. One thread trims a class at a time;
    /// takes meanwhile carve.
    #[cold]
    #[inline(never)]
    fn trim(&self) -> Trimmed {
        if self.counts.trimming.swap(true, Ordering::Acquire) {
            return Trimmed { busy: true, ..Trimmed::default() };
        }
        // SAFETY: the scratch holds `TRIM_MOST` `u32`s, committed when the class was made and
        // aligned to a page; the `trimming` flag, swapped to true above, makes this thread the
        // one that touches it until the flag is cleared below.
        let held = unsafe {
            std::slice::from_raw_parts_mut(self.scratch.base().cast::<u32>().as_ptr(), TRIM_MOST)
        };
        let goal = (KEEP / 2) as i64;
        let mut taken = 0;
        let mut warm = self.counts.warm.load(Ordering::Relaxed);
        while taken < TRIM_MOST && warm > goal {
            let step = TRIM_STEP.min(TRIM_MOST - taken);
            let popped = self.warm.pop_some(&self.links, &mut held[taken..taken + step]);
            if popped == 0 {
                break;
            }
            taken += popped;
            warm -= (popped * self.size) as i64;
        }
        let held = &mut held[..taken];
        held.sort_unstable();
        let mut trimmed = Trimmed::default();
        // Cold blocks are packed to the front of `held` as each run is read.
        let mut cold = 0;
        let mut run = 0;
        // The blocks that stay warm, chained through their links: (top, bottom).
        let mut stays: Option<(u32, u32)> = None;
        for at in 1..=taken {
            if at < taken && held[at] == held[at - 1] + 1 {
                continue;
            }
            let (first, len) = (held[run], at - run);
            // SAFETY: every block of the run came off the warm stack in this trim, so it is this
            // thread's alone and holds nothing anyone reads; it is touched again only through a
            // take off a stack, which `reuse`s a cold block first. `purge` gives back only pages
            // inside the run, none a neighbor's.
            let given = unsafe { self.region.purge(first as usize * self.size, len * self.size) };
            if given > 0 {
                held.copy_within(run..at, cold);
                cold += len;
                trimmed.given_back += given;
            } else {
                for &block in &held[run..at] {
                    match &mut stays {
                        Some((top, _)) => {
                            self.links.link(block).store(*top, Ordering::Relaxed);
                            *top = block;
                        }
                        None => {
                            self.links.link(block).store(NONE, Ordering::Relaxed);
                            stays = Some((block, block));
                        }
                    }
                }
            }
            run = at;
        }
        let bytes = (cold * self.size) as i64;
        self.cold.push_all(&held[..cold], &self.links);
        self.counts.cold.fetch_add(bytes, Ordering::Relaxed);
        self.counts.warm.fetch_sub(bytes, Ordering::Relaxed);
        if let Some((top, bottom)) = stays {
            self.warm.push_chain(top, bottom, &self.links);
        }
        let left = self.counts.warm.load(Ordering::Relaxed);
        self.counts.trim_at.store(trim_at(left), Ordering::Relaxed);
        self.counts.trimming.store(false, Ordering::Release);
        trimmed
    }

    /// Its counts now, the calling thread's gathered ones folded in by the caller.
    fn counters(&self) -> ClassCounters {
        let bytes = |count: &AtomicI64| count.load(Ordering::Relaxed).max(0) as u64;
        ClassCounters {
            size: self.size as u64,
            carved: u64::from(self.carved.load(Ordering::Relaxed).min(self.max)) * self.size as u64,
            warm: bytes(&self.counts.warm),
            cold: bytes(&self.counts.cold),
            committed: self.region.committed() as u64,
            given_back: self.counts.given_back.load(Ordering::Relaxed),
            trims: self.counts.trims.load(Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    fn carved(&self) -> u32 {
        self.carved.load(Ordering::Relaxed).min(self.max)
    }
}

/// One size class's blocks at one moment, in bytes ([`snapshot`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ClassCounters {
    /// Its blocks' size; 0 for a class not made.
    pub size: u64,
    /// Blocks carved so far: held, or given back to the class.
    pub carved: u64,
    /// Blocks given back whose pages the class keeps.
    pub warm: u64,
    /// Blocks given back whose whole pages went to the kernel.
    pub cold: u64,
    /// What its region commits: on Linux a page is backed only once touched, so of this the
    /// carved bytes, less the pages given back, are what the class can hold resident.
    pub committed: u64,
    /// Pages given back to the kernel, in all.
    pub given_back: u64,
    /// Trims of its warm blocks, in all.
    pub trims: u64,
}

impl ClassCounters {
    /// Blocks held by their owners: carved, less those given back to the class.
    pub fn live(&self) -> u64 {
        self.carved.saturating_sub(self.warm + self.cold)
    }

    /// Blocks given back to the class, warm and cold.
    pub fn free(&self) -> u64 {
        self.warm + self.cold
    }

    fn add(&mut self, other: &ClassCounters) {
        self.carved += other.carved;
        self.warm += other.warm;
        self.cold += other.cold;
        self.committed += other.committed;
        self.given_back += other.given_back;
        self.trims += other.trims;
    }
}

/// The classes there are; [`Snapshot::classes`] has one entry each.
pub const CLASS_COUNT: usize = CLASSES;

/// Every class's counts, smallest first ([`snapshot`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub classes: [ClassCounters; CLASSES],
}

impl Snapshot {
    /// The classes made so far, smallest first.
    pub fn made(&self) -> impl Iterator<Item = &ClassCounters> {
        self.classes.iter().filter(|class| class.size != 0)
    }

    /// Every class's counts summed (its `size` 0).
    pub fn total(&self) -> ClassCounters {
        let mut total = ClassCounters::default();
        for class in self.made() {
            total.add(class);
        }
        total
    }
}

impl Default for Snapshot {
    fn default() -> Snapshot {
        Snapshot { classes: [ClassCounters::default(); CLASSES] }
    }
}

/// Every class's counts now. Another thread's gathered counts reach them up to [`GATHER`] a
/// class late; the calling thread's are folded in first.
pub fn snapshot() -> Snapshot {
    let _ = tag::gathered(fold_thread);
    let mut snapshot = Snapshot::default();
    for (counters, class) in snapshot.classes.iter_mut().zip(&CLASS) {
        if let Some(Ok(class)) = class.get() {
            *counters = class.counters();
        }
    }
    snapshot
}

/// The class of the smallest block that holds `bytes` aligned to `align`.
fn class_of(bytes: usize, align: usize, tag: Tag) -> Result<usize, MemError> {
    if !align.is_power_of_two() || align > MAX_ALIGN {
        return Err(MemError::TooLarge { tag, what: "a block's alignment", wanted: align });
    }
    let size = bytes.max(align).max(SMALLEST);
    if size > LARGEST {
        return Err(MemError::TooLarge { tag, what: "a block", wanted: bytes });
    }
    // The doubling `size` falls in, and the step within it that holds it: steps of
    // `2^(shift - STEP_BITS)` from `2^shift`.
    let shift = usize::BITS - 1 - size.leading_zeros();
    let unit = 1usize << (shift - STEP_BITS);
    let step = size.div_ceil(unit) - STEPS;
    let mut index = (shift - MIN_SHIFT) as usize * STEPS + step;
    // A class of a size `align` does not divide: the next one that is aligned, at most the
    // next power of two (whose blocks are aligned to `size.next_power_of_two() >= align`).
    while class_align(index) < align {
        index += 1;
    }
    debug_assert!(class_size(index) >= size && index < CLASSES);
    Ok(index)
}

fn class(index: usize) -> Result<&'static Class, MemError> {
    let class = CLASS[index].get_or_init(|| {
        // alloc: once, when a class cannot be reserved
        Class::new(class_size(index), CLASS_BYTES, Some(index), Tag::Chunks)
            .map_err(|error| error.to_string())
    });
    match class {
        Ok(class) => Ok(class),
        Err(message) => Err(MemError::Reserve {
            tag: Tag::Chunks,
            bytes: CLASS_BYTES,
            os: std::io::Error::other(message.as_str()),
        }),
    }
}

/// A block of at least `bytes`, aligned to `align` (a power of two up to [`MAX_ALIGN`]),
/// counted to `tag` while it lives.
pub fn take(bytes: usize, align: usize, fill: Fill, tag: Tag) -> Result<Owned, MemError> {
    class(class_of(bytes, align, tag)?)?.take(fill, tag)
}

/// A block of its own, given back to its class when dropped.
pub struct Owned {
    at: NonNull<u8>,
    class: &'static Class,
    tag: Tag,
}

// SAFETY: an `Owned` is the one name of its bytes (a block is on its class's free stack or in
// exactly one `Owned`, never both), and its class is shared through atomics and a lock-free
// stack, so it may move to and be read from any thread.
unsafe impl Send for Owned {}
// SAFETY: as for `Send`: `&Owned` hands out only the address and the length.
unsafe impl Sync for Owned {}

impl Owned {
    /// The first byte, aligned to the alignment it was taken with.
    pub fn as_ptr(&self) -> NonNull<u8> {
        self.at
    }

    /// Its bytes: the size of its class, at least what was asked.
    pub fn len(&self) -> usize {
        self.class.size()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        tag::sub(self.tag, self.class.size());
        if self.class.give(self.at) {
            self.class.gather(Free::Warm, 1);
        }
    }
}

impl fmt::Debug for Owned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Owned").field("at", &self.at).field("len", &self.len()).finish()
    }
}

/// `Box<[T]>` in a block: `len` values made once, dropped with it, its block given back.
pub struct Slice<T> {
    block: Owned,
    len: usize,
    _owns: PhantomData<T>,
}

// SAFETY: a `Slice` owns its values as a `Box<[T]>` does: moving it moves them.
unsafe impl<T: Send> Send for Slice<T> {}
// SAFETY: `&Slice` hands out `&[T]` only.
unsafe impl<T: Sync> Sync for Slice<T> {}

impl<T> Slice<T> {
    /// `len` values, the one at `at` made by `make(at)`, counted to `tag`.
    pub fn new_with(
        len: usize,
        tag: Tag,
        mut make: impl FnMut(usize) -> T,
    ) -> Result<Slice<T>, MemError> {
        let bytes = len.checked_mul(mem::size_of::<T>()).ok_or(MemError::TooLarge {
            tag,
            what: "a block's values",
            wanted: len,
        })?;
        let block = take(bytes, mem::align_of::<T>(), Fill::Any, tag)?;
        let first = block.as_ptr().cast::<T>();
        let mut slice = Slice { block, len: 0, _owns: PhantomData };
        for at in 0..len {
            let value = make(at);
            // SAFETY: the block holds `len` values of `T` aligned (taken with `T`'s size times
            // `len` and its alignment), `at < len`, and the place is not yet a value: `len`
            // counts the values made so far, so a panic in `make` drops those alone.
            unsafe { first.as_ptr().wrapping_add(at).write(value) };
            slice.len = at + 1;
        }
        Ok(slice)
    }

    fn first(&self) -> NonNull<T> {
        self.block.as_ptr().cast::<T>()
    }
}

impl<T> Deref for Slice<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: the first `len` places of the block are values (`new_with`), aligned, and
        // live until the slice drops; `&self` lends them shared.
        unsafe { std::slice::from_raw_parts(self.first().as_ptr(), self.len) }
    }
}

impl<T> Drop for Slice<T> {
    fn drop(&mut self) {
        // SAFETY: the first `len` places are values no one else drops; the block, given back
        // after this, is not read again by this slice.
        unsafe {
            std::ptr::slice_from_raw_parts_mut(self.first().as_ptr(), self.len).drop_in_place()
        };
    }
}

impl<T: fmt::Debug> fmt::Debug for Slice<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The allocator of values that outlive every request and die one by one, each when its owner
/// drops it (docs/v2/plan/design/memory.md, "the image"): a parsed tree kept as a query's
/// result, a file's bytes in the VFS, a token tree a query returns. Each allocation is a block
/// of the smallest class that holds it, counted to the tag it names while it lives, so what
/// these values hold shows by subsystem and their churn never fragments the global heap,
/// which keeps only what is named on its own (`// alloc:`).
///
/// `Vec<T, Kept>`, `Box<T, Kept>`, `Arc<T, Kept>`: the allocator is a tag's byte, copied with
/// the value. A block is freed with the layout it was asked for (or any the allocator contract
/// lets a caller name for it), which picks the same class, so a block needs no header.
///
/// Counting: the tag's live bytes rise as each value is made, as a [`Region`]'s do as it
/// commits; frees and the allocation rate gather per thread ([`tag`]), as values are often
/// dropped on another thread from the one that made them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Kept(pub Tag);

impl Kept {
    pub fn tag(self) -> Tag {
        self.0
    }

    /// The allocator of a kept value made now: counted to the tag of the task running on the
    /// thread ([`crate::frame::task`]), or to `outside` when none is, so what a query's results
    /// hold shows as its subsystem's bytes.
    pub fn here(outside: Tag) -> Kept {
        match crate::frame::in_task() {
            true => Kept(crate::frame::current(|frame| frame.tag())),
            false => Kept(outside),
        }
    }

    /// The bytes a value of `layout` holds, which its tag counts: its class's size, 0 for a
    /// zero size; an error for a layout no class holds.
    pub fn held(layout: std::alloc::Layout) -> Result<usize, MemError> {
        match layout.size() {
            0 => Ok(0),
            _ => Ok(class_size(class_of(layout.size(), layout.align(), Tag::Other)?)),
        }
    }

    /// The class of a block asked for with `layout`, which a non-zero size has.
    fn class(self, layout: std::alloc::Layout) -> Result<&'static Class, MemError> {
        class(class_of(layout.size(), layout.align(), self.0)?)
    }
}

// SAFETY: a block is handed out by one `take_raw` and given back once, by `deallocate`, `grow`
// or `shrink`, to the class its layout picks, which is the class it came from: `class_of` is
// a function of the layout, and any size the contract lets a caller name for a block (from
// what was asked up to the block's length, which `allocate` returns) picks the same class.
// Blocks of one class are disjoint and live as long as the process (a class is never
// unreserved), so a block stays valid until given back, and any `Kept` (a copy) may give back
// what another took. Zero-size layouts are a dangling, aligned pointer, never given back.
unsafe impl std::alloc::Allocator for Kept {
    fn allocate(
        &self,
        layout: std::alloc::Layout,
    ) -> Result<NonNull<[u8]>, std::alloc::AllocError> {
        self.made(layout, Fill::Any)
    }

    fn allocate_zeroed(
        &self,
        layout: std::alloc::Layout,
    ) -> Result<NonNull<[u8]>, std::alloc::AllocError> {
        self.made(layout, Fill::Zeroed)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: std::alloc::Layout) {
        if layout.size() == 0 {
            return;
        }
        let class = self.class(layout).expect("a block's class, made when the block was taken");
        let warm = i32::from(class.give(ptr));
        let size = class.size();
        // The tag's freed bytes and the class's warm blocks, gathered in one thread-local
        // access; a thread being torn down has nothing gathered left: straight to the lines.
        let (freed, fold) = tag::gathered(|gathered| {
            (gathered.freed(self.0, size), class.gather_in(gathered, Free::Warm, warm))
        })
        .unwrap_or((size, warm));
        // A tag's line is never less than its live values hold: a value's bytes come off it
        // once they are freed, here or at the thread's fold.
        tag::sub(self.0, freed);
        if fold != 0 {
            class.fold(Free::Warm, fold, true);
        }
    }

    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old: std::alloc::Layout,
        new: std::alloc::Layout,
    ) -> Result<NonNull<[u8]>, std::alloc::AllocError> {
        // SAFETY: the caller's contract, passed on: `ptr` is a block of `old`, `new` no smaller.
        unsafe { self.moved(ptr, old, new, Fill::Any) }
    }

    unsafe fn grow_zeroed(
        &self,
        ptr: NonNull<u8>,
        old: std::alloc::Layout,
        new: std::alloc::Layout,
    ) -> Result<NonNull<[u8]>, std::alloc::AllocError> {
        // SAFETY: as for `grow`.
        unsafe { self.moved(ptr, old, new, Fill::Zeroed) }
    }

    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old: std::alloc::Layout,
        new: std::alloc::Layout,
    ) -> Result<NonNull<[u8]>, std::alloc::AllocError> {
        // SAFETY: as for `grow`, `new` no larger.
        unsafe { self.moved(ptr, old, new, Fill::Any) }
    }
}

// SAFETY: a copy is the same tag's byte, and any `Kept` gives back to the class a layout picks
// whatever its tag (the `Allocator` impl's comment): a block taken through one copy may be
// given back through another, moving or dropping one invalidates nothing, and cloning cannot
// unwind.
unsafe impl std::alloc::AllocatorClone for Kept {}

impl Kept {
    fn made(
        self,
        layout: std::alloc::Layout,
        fill: Fill,
    ) -> Result<NonNull<[u8]>, std::alloc::AllocError> {
        if layout.size() == 0 {
            // A dangling pointer aligned to `layout`: the contract's zero-size block.
            let at = NonNull::<u8>::without_provenance(
                std::num::NonZeroUsize::new(layout.align()).expect("an alignment is not zero"),
            );
            return Ok(NonNull::slice_from_raw_parts(at, 0));
        }
        let class = self.class(layout).map_err(|_| std::alloc::AllocError)?;
        let (at, warm) = class.take_raw(fill).map_err(|_| std::alloc::AllocError)?;
        let (size, warm) = (class.size(), -i32::from(warm));
        tag::add(self.0, size);
        // The tag's allocated bytes and the class's warm blocks, gathered in one thread-local
        // access, so a kept value's allocation writes no shared line but the tag's live bytes.
        let (allocated, fold) = tag::gathered(|gathered| {
            (gathered.allocated(self.0, size as u64), class.gather_in(gathered, Free::Warm, warm))
        })
        .unwrap_or((size as u64, warm));
        tag::add_allocated(self.0, allocated);
        if fold != 0 {
            class.fold(Free::Warm, fold, true);
        }
        Ok(NonNull::slice_from_raw_parts(at, class.size()))
    }

    /// The block of `old` at `ptr` resized to `new`: in place when `new` picks its class, else
    /// a block of `new`'s class with the bytes both hold copied, `ptr`'s given back.
    ///
    /// # Safety
    ///
    /// `ptr` is a block this allocator handed out for `old` (the `Allocator` contract's
    /// "currently allocated" and "fits"), not used again by the caller after this returns `Ok`.
    unsafe fn moved(
        self,
        ptr: NonNull<u8>,
        old: std::alloc::Layout,
        new: std::alloc::Layout,
        fill: Fill,
    ) -> Result<NonNull<[u8]>, std::alloc::AllocError> {
        if old.size() != 0 && new.size() != 0 {
            let from = self.class(old).map_err(|_| std::alloc::AllocError)?;
            let to = self.class(new).map_err(|_| std::alloc::AllocError)?;
            if std::ptr::eq(from, to) {
                if fill == Fill::Zeroed && new.size() > old.size() {
                    // SAFETY: `[old.size(), new.size())` is in the block (its class holds
                    // `new`), which the caller owns.
                    unsafe { ptr.add(old.size()).write_bytes(0, new.size() - old.size()) };
                }
                return Ok(NonNull::slice_from_raw_parts(ptr, from.size()));
            }
        }
        let made = self.made(new, fill)?;
        // SAFETY: `ptr` holds `old.size()` bytes (the caller's contract) and the new block at
        // least `new.size()`, in another block (this one is the caller's until given back
        // below), so the ranges do not overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                ptr.as_ptr(),
                made.cast::<u8>().as_ptr(),
                old.size().min(new.size()),
            )
        };
        // SAFETY: `ptr` is a block of `old`, given back once; the caller does not use it again.
        unsafe { std::alloc::Allocator::deallocate(&self, ptr, old) };
        Ok(made)
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests;
