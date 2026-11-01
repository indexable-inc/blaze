//! A reserved region: address space taken once, pages committed as the data grows, never
//! moved (docs/v2/plan/design/memory.md "Virtual memory reserved up front").
//!
//! Growing commits the next pages in place, so every address and `u32` offset into the region
//! stays valid for its life and nothing is ever copied to grow. Commits are geometric (a
//! quarter of what is committed, at least a granule, at most 64 MB), so a region that grows to
//! `n` bytes makes `O(log n)` system calls before it reaches 64 MB and one per 64 MB after.
//!
//! A region of huge pages ([`crate::huge`]) commits base pages while it is at most one huge
//! page long and whole huge pages past it, so a small one costs a few base pages and a big
//! one is huge pages from its second 2 MB on (and from its first under transparent huge
//! pages, collapsed when it outgrows it).

use std::{
    io,
    ptr::NonNull,
    sync::{PoisonError, atomic::AtomicUsize, atomic::Ordering},
};

use crate::{MemError, Tag, huge::Pages, os, tag};

/// The largest single commit: past it a region grows 64 MB per system call.
const MAX_STEP: usize = 64 << 20;

/// Held while a region of the reserved pool grows: mapping hugetlb pages over a range replaces
/// what is there, so unlike `mprotect` two growers must never map the same range. Growth of
/// such a region is rare (once per quarter of its size, at least 2 MB), so one lock for the
/// process costs nothing measurable.
#[expect(
    clippy::disallowed_types,
    reason = "a cold lock inside the allocator (brief-locks.md's exception): parking_lot's parking table would allocate from within an allocation; taken only as a region of the reserved pool grows, which a grower that waits needs done anyway"
)]
static RESERVED_GROWTH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The huge page a region of huge pages commits whole past its first.
#[inline]
fn huge_unit() -> usize {
    crate::HUGE_PAGE.max(os::page_size())
}

/// A range of reserved address space whose prefix `[base, base + committed)` is readable and
/// writable. Any thread may commit more ([`Region::ensure`]); giving pages back takes `&mut`.
pub struct Region {
    base: NonNull<u8>,
    reserved: usize,
    committed: AtomicUsize,
    /// The unit of commits below [`huge_unit`], and of all of them on base pages.
    granule: usize,
    pages: Pages,
    /// The NUMA node its pages are preferred on, once placed ([`Region::bind_node`]).
    node: Option<u32>,
    tag: Tag,
}

// SAFETY: a region is an address range and counters; commits from any thread are atomic and
// idempotent (`ensure`), and what lives in the range is the owner's to synchronize.
unsafe impl Send for Region {}
// SAFETY: as for `Send`: `&Region` only commits, which any number of threads may do at once.
unsafe impl Sync for Region {}

impl Region {
    /// Reserves `bytes` (rounded up to whole pages) for `tag`, committing nothing.
    pub fn reserve(bytes: usize, tag: Tag) -> Result<Region, MemError> {
        Region::placed(bytes, Pages::Base, None, tag)
    }

    /// As [`Region::reserve`], made of the huge pages this process uses ([`crate::huge`]):
    /// for the big, randomly read regions (frames, interners, memo and node tables, the
    /// store's index). Base pages while it is at most 2 MB long, whole huge pages past it.
    /// Errs when `BLAZE_HUGE_PAGES` names pages the system cannot give.
    pub fn reserve_huge(bytes: usize, tag: Tag) -> Result<Region, MemError> {
        Region::placed(bytes, Region::chosen(bytes, tag)?, None, tag)
    }

    /// As [`Region::reserve`], made of `pages` whatever the process chose: to compare them in
    /// one process (the tests, the bench). [`Pages::Reserved`] is an error but on Linux.
    pub fn reserve_pages(bytes: usize, pages: Pages, tag: Tag) -> Result<Region, MemError> {
        if pages == Pages::Reserved {
            os::reserved_pool().map_err(|os| MemError::Reserve { tag, bytes, os })?;
        }
        Region::placed(bytes, pages, None, tag)
    }

    /// As [`Region::reserve`], its pages preferred on NUMA node `node` from the first
    /// (see [`Region::bind_node`] for what each system does).
    pub fn reserve_on(bytes: usize, node: u32, tag: Tag) -> Result<Region, MemError> {
        Region::placed(bytes, Pages::Base, Some(node), tag)
    }

    /// As [`Region::reserve_huge`], its pages preferred on NUMA node `node` from the first.
    pub fn reserve_huge_on(bytes: usize, node: u32, tag: Tag) -> Result<Region, MemError> {
        Region::placed(bytes, Region::chosen(bytes, tag)?, Some(node), tag)
    }

    fn chosen(bytes: usize, tag: Tag) -> Result<Pages, MemError> {
        crate::huge::chosen().map_err(|os| MemError::Reserve { tag, bytes, os })
    }

    fn placed(bytes: usize, pages: Pages, node: Option<u32>, tag: Tag) -> Result<Region, MemError> {
        let granule = os::page_size().max(64 << 10);
        let whole = if pages.is_huge() { huge_unit() } else { granule };
        let reserved = crate::align::checked_align_up(bytes.max(1), whole).ok_or(
            MemError::TooLarge { tag, what: "a reservation", wanted: bytes },
        )?;
        let transparent = pages == Pages::Transparent;
        let base = match node {
            Some(node) => os::reserve_on(reserved, transparent, node),
            None => os::reserve(reserved, transparent),
        }
        .map_err(|os| MemError::Reserve { tag, bytes: reserved, os })?;
        Ok(Region { base, reserved, committed: AtomicUsize::new(0), granule, pages, node, tag })
    }

    /// The unit a commit or decommit ending at `len` is rounded to: whole huge pages past the
    /// first on huge pages, the granule below it and on base pages.
    #[inline]
    fn unit(&self, len: usize) -> usize {
        if self.pages.is_huge() && len > huge_unit() { huge_unit() } else { self.granule }
    }

    /// Prefers NUMA node `node` for the region's pages not yet made, so a worker's memory is
    /// on its own node. Per system, with no other path when it cannot:
    ///
    /// - Linux: `mbind` with `MPOL_PREFERRED` over the whole reservation (pages go to `node`
    ///   while it has room, elsewhere after); a kernel without NUMA has node 0 alone.
    /// - Windows: a range's node is chosen when it is reserved ([`Region::reserve_on`]), so
    ///   this is an error unless the region was reserved on `node` or the machine has that one
    ///   node.
    /// - macOS (and targets without virtual memory): one memory node; node 0 is where the
    ///   memory is, any other an error.
    ///
    /// Pages already made stay where they are. A decommit keeps the placement.
    pub fn bind_node(&mut self, node: u32) -> Result<(), MemError> {
        if self.node == Some(node) {
            return Ok(());
        }
        // SAFETY: the range is this region's whole reservation, alive while `self` is, and a
        // policy change touches no data.
        unsafe { os::bind(self.base, self.reserved, node) }.map_err(|os| MemError::Reserve {
            tag: self.tag,
            bytes: self.reserved,
            os,
        })?;
        self.node = Some(node);
        Ok(())
    }

    /// The NUMA node its pages are preferred on, if placed.
    pub fn node(&self) -> Option<u32> {
        self.node
    }

    pub fn base(&self) -> NonNull<u8> {
        self.base
    }

    /// The address `offset` bytes into the reservation (`offset <= reserved`).
    #[inline]
    pub fn at(&self, offset: usize) -> NonNull<u8> {
        debug_assert!(offset <= self.reserved);
        // SAFETY: `base` is not null and `base + offset` stays inside the reservation, which
        // the kernel gave whole, so it does not wrap around the address space.
        unsafe { NonNull::new_unchecked(self.base.as_ptr().wrapping_add(offset)) }
    }

    pub fn reserved(&self) -> usize {
        self.reserved
    }

    /// Bytes from the base that are readable and writable now.
    pub fn committed(&self) -> usize {
        self.committed.load(Ordering::Acquire)
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }

    pub fn is_huge(&self) -> bool {
        self.pages.is_huge()
    }

    /// What its pages are.
    pub fn pages(&self) -> Pages {
        self.pages
    }

    /// Makes at least `[base, base + len)` readable and writable. Lock-free but on the
    /// reserved pool: threads that grow at once may commit overlapping pages, which is
    /// harmless, and each byte is counted once. Errs when `len` passes the reservation or the
    /// kernel refuses.
    #[inline]
    pub fn ensure(&self, len: usize) -> Result<(), MemError> {
        if len <= self.committed.load(Ordering::Acquire) {
            return Ok(());
        }
        self.grow(len)
    }

    /// What [`Region::ensure`]`(len)` would leave committed, by the same geometric rule, as
    /// the region stands now: for an owner that reserves the growth from a limit first.
    pub fn committed_after(&self, len: usize) -> Result<usize, MemError> {
        if len > self.reserved {
            return Err(MemError::Exhausted { tag: self.tag, reserved: self.reserved, wanted: len });
        }
        let from = self.committed.load(Ordering::Acquire);
        if len <= from {
            return Ok(from);
        }
        let step = (from / 4).clamp(self.granule, MAX_STEP.max(self.granule));
        let want = len.max(from + step);
        Ok(crate::align::checked_align_up(want, self.unit(want))
            .unwrap_or(self.reserved)
            .min(self.reserved))
    }

    #[cold]
    fn grow(&self, len: usize) -> Result<(), MemError> {
        if self.pages == Pages::Reserved {
            let _one = RESERVED_GROWTH.lock().unwrap_or_else(PoisonError::into_inner);
            return self.grow_reserved(len);
        }
        let from = self.committed.load(Ordering::Acquire);
        let target = self.committed_after(len)?;
        if target <= from {
            return Ok(());
        }
        let start = crate::align::align_down(from, self.granule);
        // SAFETY: `[start, target)` is granule-aligned (so page-aligned) and inside the
        // reservation (`target <= reserved`), which lives as long as `self`.
        unsafe { os::commit(self.at(start), target - start) }.map_err(|os| {
            MemError::Commit { tag: self.tag, bytes: target - start, os }
        })?;
        let unit = huge_unit();
        if self.pages == Pages::Transparent && from > 0 && from < unit && target > unit {
            // SAFETY: `[base, base + unit)` is huge-aligned (the reservation's base is) and
            // committed (`target > unit`), inside the reservation.
            unsafe { os::collapse(self.base, unit) };
        }
        self.publish(target);
        Ok(())
    }

    /// [`Region::grow`] on the reserved pool, under [`RESERVED_GROWTH`]: the first huge page's
    /// worth in base pages, each huge page past it mapped once from the pool.
    fn grow_reserved(&self, len: usize) -> Result<(), MemError> {
        let from = self.committed.load(Ordering::Acquire);
        let target = self.committed_after(len)?;
        if target <= from {
            return Ok(());
        }
        let unit = huge_unit();
        let start = crate::align::align_down(from, self.granule);
        let base_end = target.min(unit);
        if start < base_end {
            // SAFETY: `[start, base_end)` is granule-aligned and inside the reservation; the
            // first huge page's worth is never mapped from the pool, so this only changes the
            // protection of reserved space.
            unsafe { os::commit(self.at(start), base_end - start) }.map_err(|os| {
                MemError::Commit { tag: self.tag, bytes: base_end - start, os }
            })?;
        }
        let huge_from = from.max(unit);
        if target > huge_from {
            debug_assert!(huge_from % unit == 0 && target % unit == 0);
            // SAFETY: `[huge_from, target)` is huge-aligned (committed lengths past one huge
            // page are whole huge pages, `unit`), inside the reservation and not committed
            // (`huge_from >= from`), so nothing reaches it; the lock held makes this the only
            // commit of it.
            unsafe { os::commit_reserved(self.at(huge_from), target - huge_from, self.node) }
                .map_err(|os| MemError::Commit { tag: self.tag, bytes: target - huge_from, os })?;
        }
        self.publish(target);
        Ok(())
    }

    /// Publishes `[base, base + target)` as committed and counts what it adds: counted before
    /// it is published, so a thread whose `ensure` sees the new length (and hands the bytes on
    /// to another tag, as `chunks::take` does) comes after the count, and a tag's live bytes
    /// never fall below zero. A grower that loses the race takes its count back; meanwhile the
    /// high-water mark may read high.
    fn publish(&self, target: usize) {
        let mut before = self.committed.load(Ordering::Acquire);
        while target > before {
            tag::add(self.tag, target - before);
            // Release: the commit, and the count, are ordered before a reader that sees the
            // new length.
            match self.committed.compare_exchange_weak(
                before,
                target,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    tag::committed(target - before);
                    return;
                }
                Err(now) => {
                    tag::sub(self.tag, target - before);
                    before = now;
                }
            }
        }
    }

    /// Gives every page past the first `keep` bytes (rounded up to a granule, or to a huge page
    /// past the first on huge pages) back to the kernel: what a frame or table does after a
    /// burst, so resident memory returns to the working set.
    pub fn decommit_after(&mut self, keep: usize) -> io::Result<()> {
        let keep = crate::align::checked_align_up(keep, self.unit(keep))
            .unwrap_or(self.reserved)
            .min(self.reserved);
        let committed = *self.committed.get_mut();
        if keep >= committed {
            return Ok(());
        }
        // SAFETY: `[keep, committed)` is aligned to its unit (huge-aligned where it covers pool
        // pages, which start at a huge page and end at a committed length past it), inside the
        // reservation, and `&mut self` says nothing borrows the region's memory.
        unsafe { os::decommit(self.at(keep), committed - keep, self.pages == Pages::Transparent) }?;
        *self.committed.get_mut() = keep;
        tag::sub(self.tag, committed - keep);
        tag::decommitted(committed - keep);
        #[cfg(target_os = "linux")]
        if let Some(node) = self.node {
            // SAFETY: the range just mapped afresh, inside this live reservation and holding no
            // page; the new mapping lost the placement, which this puts back.
            unsafe { os::bind(self.at(keep), committed - keep, node) }?;
        }
        Ok(())
    }

    /// Lets the kernel reclaim the pages of `[from, from + len)` (rounded inward to pages)
    /// while they stay committed; their contents become unspecified. Pages of the reserved
    /// pool are kept: one given back would be faulted in again on the next touch, which kills
    /// the process when the pool has none free.
    ///
    /// # Safety
    ///
    /// Nothing reads the range's contents again before writing them.
    pub unsafe fn purge(&self, from: usize, len: usize) {
        let page = os::page_size();
        let Some(start) = crate::align::checked_align_up(from, page) else { return };
        let mut end = crate::align::align_down(from.saturating_add(len), page).min(self.committed());
        if self.pages == Pages::Reserved {
            end = end.min(huge_unit());
        }
        if end > start {
            // SAFETY: `[start, end)` is page-aligned, committed, and the caller says nothing
            // holds data in it.
            unsafe { os::purge(self.at(start), end - start) };
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        let committed = *self.committed.get_mut();
        tag::sub(self.tag, committed);
        tag::decommitted(committed);
        // SAFETY: `base` and `reserved` are this reservation's own, and the region is gone, so
        // nothing reaches its memory.
        unsafe { os::release(self.base, self.reserved) };
    }
}

impl std::fmt::Debug for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Region")
            .field("tag", &self.tag)
            .field("reserved", &self.reserved)
            .field("committed", &self.committed())
            .field("pages", &self.pages)
            .field("node", &self.node)
            .finish()
    }
}
