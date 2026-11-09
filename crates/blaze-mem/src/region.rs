//! A reserved region: address space taken once, pages committed as the data grows, never
//! moved (docs/v2/plan/design/memory.md "Virtual memory reserved up front").
//!
//! Growing commits the next pages in place, so every address and `u32` offset into the region
//! stays valid for its life and nothing is ever copied to grow. Commits are geometric (a
//! quarter of what is committed, at least a granule, at most 64 MB), so a region that grows to
//! `n` bytes makes `O(log n)` system calls before it reaches 64 MB and one per 64 MB after.
//! On Linux the reservation is readable and writable from the start, so growing publishes the
//! new length first and then prefaults it (`os::commit`): no `mprotect`, so no writer of the
//! process's `mmap_lock` that every other thread's faults would queue behind, and no worker
//! waits for the pages, which the one grower makes in a batch while the rest go on.
//!
//! A region of huge pages ([`crate::huge`]) commits base pages while it is at most one huge
//! page long and whole huge pages past it, so a small one costs a few base pages and a big
//! one is huge pages from its second 2 MB on (and from its first under transparent huge
//! pages, collapsed when it outgrows it).

use std::{
    io,
    ops::Range,
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
    reason = "below blaze-core: blaze-mem is a dependency of blaze-core; a cold lock inside the allocator (brief-locks.md's exception): parking_lot's parking table would allocate from within an allocation; taken only as a region of the reserved pool grows, which a grower that waits needs done anyway"
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
    /// Made by [`Region::reserve_sparse`]: every byte readable and writable from the start, and
    /// `committed` the prefix its owner said it uses.
    sparse: bool,
    /// Of a sparse region counted by ranges ([`Region::reuse`], [`Region::evict`]), the bytes
    /// made the process's and not given back since: what its tag holds of it besides
    /// `committed`, which such a region leaves at what its reservation counted.
    held: AtomicUsize,
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

    /// A sparse region: every byte of `bytes` (rounded up to whole granules) readable and
    /// writable from the start, reading zeros until written, and only the pages touched ever
    /// backed by memory (Linux and macOS; base pages, never huge ones). [`Region::ensure`] does
    /// no system call here: it counts the prefix the owner says it writes as committed, so
    /// the tag holds what the table uses, not what it could reach. For a table read at any
    /// index but written in a few (the query database's page directories): a read past the
    /// committed prefix is valid and finds zero. Windows commits it whole, and counts it so.
    /// Never decommitted ([`Region::decommit_after`] refuses).
    ///
    /// Its owner counts what it uses one of two ways, never both: a prefix ([`Region::ensure`]),
    /// or ranges anywhere in it, each made the process's with [`Region::reuse`] and given back
    /// with [`Region::evict`] (the store's buffer pool, docs/v2/plan/design/store-pool.md, whose
    /// windows are slots of 1 GiB in a reservation of terabytes). Mixing them panics, naming the
    /// region.
    pub fn reserve_sparse(bytes: usize, tag: Tag) -> Result<Region, MemError> {
        let granule = os::page_size().max(64 << 10);
        let reserved = crate::align::checked_align_up(bytes.max(1), granule)
            .ok_or(MemError::TooLarge { tag, what: "a reservation", wanted: bytes })?;
        let base = os::reserve_sparse(reserved).map_err(|os| MemError::Reserve {
            tag,
            bytes: reserved,
            os,
        })?;
        tag::reserved(tag, reserved);
        tag::base_pages(reserved);
        let region = Region {
            base,
            reserved,
            committed: AtomicUsize::new(0),
            granule,
            pages: Pages::Base,
            node: None,
            sparse: true,
            held: AtomicUsize::new(0),
            tag,
        };
        if cfg!(windows) {
            region.publish(reserved);
        }
        Ok(region)
    }

    fn chosen(bytes: usize, tag: Tag) -> Result<Pages, MemError> {
        crate::huge::chosen().map_err(|os| MemError::Reserve { tag, bytes, os })
    }

    fn placed(bytes: usize, pages: Pages, node: Option<u32>, tag: Tag) -> Result<Region, MemError> {
        let granule = os::page_size().max(64 << 10);
        let whole = if pages.is_huge() { huge_unit() } else { granule };
        let reserved = crate::align::checked_align_up(bytes.max(1), whole)
            .ok_or(MemError::TooLarge { tag, what: "a reservation", wanted: bytes })?;
        let transparent = pages == Pages::Transparent;
        let base = match node {
            Some(node) => os::reserve_on(reserved, transparent, node),
            None => os::reserve(reserved, transparent),
        }
        .map_err(|os| MemError::Reserve { tag, bytes: reserved, os })?;
        tag::reserved(tag, reserved);
        if pages == Pages::Base {
            tag::base_pages(reserved);
        }
        Ok(Region {
            base,
            reserved,
            committed: AtomicUsize::new(0),
            granule,
            pages,
            node,
            sparse: false,
            held: AtomicUsize::new(0),
            tag,
        })
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

    /// The address `offset` bytes into the reservation.
    ///
    /// # Panics
    ///
    /// When `offset > reserved`, naming both and the region's tag. The check runs in every
    /// build: this is a safe fn, and past the reservation `base + offset` could wrap to null.
    #[inline]
    #[track_caller]
    pub fn at(&self, offset: usize) -> NonNull<u8> {
        if offset > self.reserved {
            self.past_reservation(offset);
        }
        // SAFETY: `base` is not null and `base + offset` stays inside the reservation (checked
        // just above), which the kernel gave whole, so it does not wrap around the address
        // space.
        unsafe { NonNull::new_unchecked(self.base.as_ptr().wrapping_add(offset)) }
    }

    /// [`Region::at`] without its check, for a caller whose own bound already implies it, on a
    /// path where the compare shows: the query database's `Dense::place`, `Dense::get_mut` and
    /// `Dense::slice`, whose indexes are below a length it reserved for.
    ///
    /// # Safety
    ///
    /// `offset <= self.reserved()`.
    #[inline(always)]
    pub unsafe fn at_unchecked(&self, offset: usize) -> NonNull<u8> {
        debug_assert!(offset <= self.reserved);
        // SAFETY: `base` is not null and `base + offset` stays inside the reservation (the
        // caller's contract), which the kernel gave whole, so it does not wrap around.
        unsafe { NonNull::new_unchecked(self.base.as_ptr().wrapping_add(offset)) }
    }

    #[cold]
    #[inline(never)]
    #[track_caller]
    fn past_reservation(&self, offset: usize) -> ! {
        panic!(
            "byte {offset} of a {:?} region of {} reserved bytes: past its reservation",
            self.tag, self.reserved,
        )
    }

    pub fn reserved(&self) -> usize {
        self.reserved
    }

    /// Bytes from the base that are committed: readable and writable now (of a sparse region,
    /// [`Region::reserve_sparse`], the prefix its owner uses; every byte of one is readable).
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
            return Err(MemError::Exhausted {
                tag: self.tag,
                reserved: self.reserved,
                wanted: len,
            });
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
        if self.sparse {
            if self.held.load(Ordering::Acquire) != 0 {
                self.mixed("committing a prefix of");
            }
            // Readable and writable already: only the count moves.
            self.publish(target);
            return Ok(());
        }
        if os::RESERVATION_WRITABLE {
            // Readable and writable already (Linux, `os::reserve`), so the length is published
            // first and the pages it adds prefaulted after, by the one thread whose publish
            // moved it: a grower that lost the race, and every thread that touches the new
            // pages meanwhile, goes on (a touch before the prefault faults its page in, which
            // the prefault then leaves alone). Past its first 2 MiB a huge region is advised
            // huge pages already, so each 2 MiB's first touch makes one; its first 2 MiB, base
            // pages while small, is advised and collapsed once, as it grows past them.
            if let Some(before) = self.publish(target) {
                let start = crate::align::align_down(before, self.granule);
                // SAFETY: `[start, target)` is granule-aligned (so page-aligned) and inside the
                // reservation (`target <= reserved`), which lives as long as `self`.
                unsafe { os::commit(self.at(start), target - start) }.map_err(|os| {
                    MemError::Commit { tag: self.tag, bytes: target - start, os }
                })?;
                let unit = huge_unit();
                if self.pages == Pages::Transparent && before < unit && target > unit {
                    // SAFETY: `[base, base + unit)` is huge-aligned (the reservation's base is)
                    // and committed (`target > unit`), inside the reservation.
                    unsafe { os::promote_first(self.base, unit) };
                }
            }
            return Ok(());
        }
        let start = crate::align::align_down(from, self.granule);
        // SAFETY: `[start, target)` is granule-aligned (so page-aligned) and inside the
        // reservation (`target <= reserved`), which lives as long as `self`.
        unsafe { os::commit(self.at(start), target - start) }.map_err(|os| MemError::Commit {
            tag: self.tag,
            bytes: target - start,
            os,
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
            unsafe { os::commit(self.at(start), base_end - start) }
                .map_err(|os| MemError::Commit { tag: self.tag, bytes: base_end - start, os })?;
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
    /// high-water mark may read high. Says what was committed before, when this call moved it.
    fn publish(&self, target: usize) -> Option<usize> {
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
                    return Some(before);
                }
                Err(now) => {
                    tag::sub(self.tag, target - before);
                    before = now;
                }
            }
        }
        None
    }

    /// Gives every page past the first `keep` bytes (rounded up to a granule, or to a huge page
    /// past the first on huge pages) back to the kernel: what a frame or table does after a
    /// burst, so resident memory returns to the working set.
    ///
    /// # Errors
    ///
    /// On a sparse region ([`Region::reserve_sparse`]), whose bytes must stay readable: its
    /// owner reads past the committed prefix.
    pub fn decommit_after(&mut self, keep: usize) -> io::Result<()> {
        if self.sparse {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                // alloc: the error of a refused decommit, once
                format!(
                    "a sparse {:?} region of {} bytes is never decommitted: its owner reads past what it committed",
                    self.tag, self.reserved
                ),
            ));
        }
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
        let remapped = unsafe { os::decommit(self.at(keep), committed - keep, self.pages) }?;
        *self.committed.get_mut() = keep;
        tag::sub(self.tag, committed - keep);
        tag::decommitted(committed - keep);
        #[cfg(target_os = "linux")]
        if remapped && let Some(node) = self.node {
            // SAFETY: the range just mapped afresh, inside this live reservation and holding no
            // page; the new mapping lost the placement, which this puts back.
            unsafe { os::bind(self.at(keep), committed - keep, node) }?;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = remapped;
        Ok(())
    }

    /// Lets the kernel reclaim the pages of `[from, from + len)` (rounded inward to pages)
    /// while they stay reserved; their contents become unspecified, and [`Region::reuse`] makes
    /// them the process's again before they are touched. Pages of the reserved pool are kept:
    /// one given back would be faulted in again on the next touch, which kills the process
    /// when the pool has none free. Returns the bytes given back: 0 when the kernel refused
    /// them, or the target gives none back, so a caller never counts resident pages as gone.
    ///
    /// # Safety
    ///
    /// Nothing reads the range's contents again before writing them, and nothing touches it
    /// before [`Region::reuse`] is called over it.
    pub unsafe fn purge(&self, from: usize, len: usize) -> usize {
        let page = os::page_size();
        let Some(start) = crate::align::checked_align_up(from, page) else { return 0 };
        let mut end =
            crate::align::align_down(from.saturating_add(len), page).min(self.committed());
        if self.pages == Pages::Reserved {
            end = end.min(huge_unit());
        }
        if end <= start {
            return 0;
        }
        // SAFETY: `[start, end)` is page-aligned, committed, and the caller says nothing
        // holds data in it.
        if unsafe { os::purge(self.at(start), end - start) } { end - start } else { 0 }
    }

    /// Makes `[from, from + len)` (rounded out to pages), which [`Region::purge`] may have given
    /// back in part, the process's again before it is written: macOS counts the pages back
    /// into the footprint, Windows commits them; elsewhere nothing to do. An error when the
    /// kernel refuses (Windows' commit limit).
    ///
    /// Of a sparse region ([`Region::reserve_sparse`]) counted by ranges, it is how a range
    /// becomes the owner's: any range of the reservation, page-aligned at both ends, counted to
    /// the region's tag from now until [`Region::evict`] gives it back. On macOS the pages,
    /// which an eviction marked reusable, count in the footprint again before they are written
    /// (`MADV_FREE_REUSE`), and their bytes are unspecified, so its owner writes all of them
    /// (the pool's loader zeroes what a short read left, [`crate::racy::zero`]); on Linux they
    /// read zeros until written. Its owner reuses a range it does not hold: one held already
    /// would be counted twice.
    ///
    /// # Panics
    ///
    /// When the range passes what is committed, naming both and the region's tag: only pages
    /// once committed are ever purged. Of a sparse region, when the range is not page-aligned,
    /// passes the reservation, or the region is counted by its prefix ([`Region::ensure`]),
    /// naming the region.
    #[track_caller]
    pub fn reuse(&self, from: usize, len: usize) -> Result<(), MemError> {
        if self.sparse {
            return self.reuse_range(from, len);
        }
        let page = os::page_size();
        let start = crate::align::align_down(from, page);
        let end =
            crate::align::checked_align_up(from.saturating_add(len), page).unwrap_or(usize::MAX);
        let committed = self.committed();
        assert!(
            end <= committed,
            "reusing bytes {from} to {} of a {:?} region with {committed} committed: past what it committed",
            from.saturating_add(len),
            self.tag,
        );
        if end <= start {
            return Ok(());
        }
        // SAFETY: `[start, end)` is page-aligned, inside the committed prefix of this live
        // reservation (checked just above).
        unsafe { os::reuse(self.at(start), end - start) }.map_err(|os| MemError::Commit {
            tag: self.tag,
            bytes: end - start,
            os,
        })
    }
}

impl Region {
    /// [`Region::reuse`] of a sparse region: counts the range and makes it the process's.
    #[track_caller]
    fn reuse_range(&self, from: usize, len: usize) -> Result<(), MemError> {
        let end = self.checked_range(from, len, "reusing");
        if end == from {
            return Ok(());
        }
        self.by_ranges("reusing");
        self.held.fetch_add(len, Ordering::AcqRel);
        tag::add(self.tag, len);
        tag::committed(len);
        // SAFETY: `[from, end)` is page-aligned inside this live reservation (checked above),
        // every byte of which is mapped readable and writable.
        if let Err(os) = unsafe { os::reuse(self.at(from), len) } {
            self.held.fetch_sub(len, Ordering::AcqRel);
            tag::sub(self.tag, len);
            tag::decommitted(len);
            return Err(MemError::Commit { tag: self.tag, bytes: len, os });
        }
        Ok(())
    }

    /// Gives the pages of `ranges` (byte offsets, each page-aligned at both ends) of a sparse
    /// region counted by ranges back to the kernel, and says how many bytes went: the store's
    /// pool evicting a batch of pages no one pins (docs/v2/plan/design/store-pool.md "Giving
    /// pages back"). The region's tag falls by those bytes now, as the system's own count does:
    ///
    /// - Linux: `MADV_DONTNEED`, every 256 ranges one `process_madvise` of this process, so a
    ///   batch costs one TLB flush. The pages read zeros when next touched.
    /// - macOS: `MADV_FREE_REUSABLE` per range, out of `phys_footprint` at once; their bytes are
    ///   unspecified until [`Region::reuse`] and a write.
    /// - Windows: `MEM_DECOMMIT` per range. A decommitted page faults when read, so there an
    ///   optimistic read ([`crate::racy::copy_out`]) of a page being given back is not defined;
    ///   the pool on Windows is not designed yet.
    ///
    /// Never plain `MADV_FREE`, which the kernel goes on counting (rule `one-memory-measure`).
    ///
    /// The ranges go in order, and the kernel's refusal stops the call: the bytes given back are
    /// those of the ranges of a prefix of `ranges`, each whole, and the rest stay resident and
    /// counted. That costs memory, not correctness, and the owner finds which went by summing
    /// lengths. Adjacent ranges are best merged by the owner first: a range is an `iovec`.
    ///
    /// # Errors
    ///
    /// [`MemError::Evict`] when this system cannot give the pages back at all, checked once per
    /// process (`os::check_evict`): on Linux a kernel without `process_madvise` of any advice
    /// for its own process. Nothing is given back then.
    ///
    /// # Panics
    ///
    /// When the region is not sparse (a committed prefix is given back with
    /// [`Region::purge`] or [`Region::decommit_after`]), is counted by its prefix, a range is
    /// not page-aligned or passes the reservation, or the ranges come to more than the region
    /// holds; each names the region.
    ///
    /// # Safety
    ///
    /// Nothing holds a reference into the ranges or reads them but through
    /// [`crate::racy::copy_out`] (the pool's pins are none, its state words `Evicting`), and
    /// each range was made the process's with [`Region::reuse`] and not given back since.
    #[track_caller]
    pub unsafe fn evict(&self, ranges: &[Range<usize>]) -> Result<usize, MemError> {
        if !self.sparse {
            panic!(
                "evicting pages of a {:?} region of {} reserved bytes that is not sparse: give a \
                 committed prefix back with purge or decommit_after",
                self.tag, self.reserved,
            );
        }
        self.by_ranges("evicting");
        let mut asked = 0usize;
        for range in ranges {
            self.checked_range(range.start, range.len(), "evicting");
            asked += range.len();
        }
        let held = self.held.load(Ordering::Acquire);
        assert!(
            asked <= held,
            "evicting {asked} bytes of a {:?} region of {} reserved bytes that holds {held}: a \
             range given back twice, or never reused",
            self.tag,
            self.reserved,
        );
        if asked == 0 {
            return Ok(0);
        }
        os::check_evict().map_err(|os| MemError::Evict { tag: self.tag, os })?;
        // SAFETY: each range is page-aligned inside this live sparse reservation (checked
        // above), and the caller says nothing references or reads them but atomically.
        let given = unsafe { os::evict(self.base, ranges) };
        self.held.fetch_sub(given, Ordering::AcqRel);
        tag::sub(self.tag, given);
        tag::decommitted(given);
        Ok(given)
    }

    /// The bytes of a sparse region counted by ranges that are the process's now: what
    /// [`Region::reuse`] made so and [`Region::evict`] has not given back.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// The end of `[from, from + len)`, checked page-aligned and inside the reservation.
    #[track_caller]
    fn checked_range(&self, from: usize, len: usize, doing: &str) -> usize {
        let page = os::page_size();
        let end = from.checked_add(len).filter(|&end| end <= self.reserved);
        match end {
            Some(end) if from % page == 0 && end % page == 0 => end,
            _ => panic!(
                "{doing} bytes {from} to {} of a sparse {:?} region of {} reserved bytes: a range \
                 must be whole pages of {page} bytes inside the reservation",
                from.saturating_add(len),
                self.tag,
                self.reserved,
            ),
        }
    }

    /// Panics unless this sparse region is counted by ranges: nothing committed past what its
    /// reservation counted (all of it on Windows, none elsewhere).
    #[track_caller]
    fn by_ranges(&self, doing: &str) {
        let initial = if cfg!(windows) { self.reserved } else { 0 };
        if self.committed() != initial {
            self.mixed(doing);
        }
    }

    #[cold]
    #[inline(never)]
    #[track_caller]
    fn mixed(&self, doing: &str) -> ! {
        panic!(
            "{doing} a sparse {:?} region of {} reserved bytes counted both by its prefix ({} \
             committed) and by ranges ({} held): its owner counts one way",
            self.tag,
            self.reserved,
            self.committed(),
            self.held(),
        )
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        let held = *self.held.get_mut();
        tag::sub(self.tag, held);
        tag::decommitted(held);
        let committed = *self.committed.get_mut();
        tag::sub(self.tag, committed);
        tag::decommitted(committed);
        // SAFETY: `base` and `reserved` are this reservation's own, and the region is gone, so
        // nothing reaches its memory.
        unsafe { os::release(self.base, self.reserved) };
        tag::released(self.tag, self.reserved);
        if self.pages == Pages::Base {
            tag::base_pages_released(self.reserved);
        }
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
            .field("sparse", &self.sparse)
            .field("held", &self.held())
            .finish()
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests;
