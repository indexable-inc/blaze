//! Pools: a slab per type with free lists, a cache per worker in front, refilled and returned
//! 64 objects at a time (docs/v2/plan/design/memory.md, "fixed-size objects that come and
//! go").
//!
//! A [`Pool<T>`] owns a reserved region of slots that never moves, so a slot's address and its
//! `u32` index are stable. Each worker allocates and frees through a [`Cache`] of its own: a
//! plain array of free indices, no atomic. When it runs dry it takes a batch of 64 from the
//! pool's lock-free stack of full batches (one compare-and-swap), and when it holds 128 it
//! returns 64 the same way. A [`Pooled`] dropped away from any cache returns its one slot to the
//! pool's stack of strays, which caches drain before carving new slots. Slots go back to the
//! pool, never to the kernel: the warm loop makes no system call and no global allocation.
//!
//! Objects readers may still hold when they are retired (under [`blaze_reclaim`] guards) go
//! to a [`Limbo`], which keeps them in their slots and returns them to the pool once their
//! grace has passed: no box per retirement and no list that grows in the warm loop.
//!
//! Cost: an allocation or free through a cache is an array push or pop and a write of the
//! value; one compare-and-swap per 64 of them on the full stack. The batch's descriptor stays
//! with the cache that emptied it and fills its next spill; descriptors move between caches
//! four at a time, so the empty stack costs a quarter of a compare-and-swap per batch.

use std::{
    fmt,
    marker::PhantomData,
    mem,
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
};

use blaze_reclaim::Grace;

use crate::{
    MemError, Region, RegionVec, Tag,
    error::fail,
    stack::{LinkTable, Stack},
    sync::atomic::{AtomicU32, Ordering},
    tag,
};

/// Objects moved between a cache and the pool at once.
pub const BATCH: usize = 64;

const CACHE: usize = 2 * BATCH;

/// Empty batch descriptors a cache keeps, so moving a batch costs the one compare-and-swap of
/// the full stack: a refill keeps the descriptor it emptied, a spill fills one it keeps.
const STASH: usize = 8;

/// Descriptors moved between a cache's stash and the pool's empty stack at once (one
/// compare-and-swap): half the stash, so a cache that only refills or only spills touches the
/// empty stack once per `MOVE` batches.
const MOVE: usize = STASH / 2;

/// A batch of free slot indices, one compare-and-swap to move.
#[repr(C)]
struct Batch {
    items: [u32; BATCH],
}

/// Descriptors beyond the most full batches the slots can make, for those in flight between a
/// cache and a stack and those caches keep empty ([`STASH`] each): room for 2,048 caches of a
/// pool, 4 MB of address space, committed only as descriptors are made.
const SPARE_BATCHES: u32 = 16_384;

/// A slab of `T`s with free lists, shared by every worker; allocate through [`Pool::cache`].
pub struct Pool<T> {
    slots: Region,
    max: u32,
    carved: AtomicU32,
    slot_links: LinkTable,
    /// Slots freed one at a time, away from any cache.
    stray: Stack,
    batches: Region,
    max_batches: u32,
    batches_carved: AtomicU32,
    batch_links: LinkTable,
    full: Stack,
    empty: Stack,
    tag: Tag,
    _owns: PhantomData<T>,
}

// SAFETY: the pool's shared state is atomics and lock-free stacks; a slot is reached only by
// the one `Pooled` that owns it, and values move between threads with their `Pooled`, hence
// `T: Send`.
unsafe impl<T: Send> Sync for Pool<T> {}
// SAFETY: as for `Sync`.
unsafe impl<T: Send> Send for Pool<T> {}

impl<T> Pool<T> {
    const SIZE: usize = {
        assert!(mem::size_of::<T>() != 0, "a pool of a zero-sized type holds nothing");
        assert!(mem::align_of::<T>() <= 4096, "a pool's slots align to at most a page");
        mem::size_of::<T>()
    };

    /// Slots carved (and committed) at once: a batch, or as many as fit in 2 MiB when slots
    /// are large, so a pool of big slots commits no more than its first few objects need.
    const CARVE: u32 = {
        let fit = (2 << 20) / Self::SIZE;
        if fit < 1 {
            1
        } else if fit > BATCH {
            BATCH as u32
        } else {
            fit as u32
        }
    };

    /// A pool of at most `max` objects for `tag`, reserving their room and committing none.
    pub fn new(max: u32, tag: Tag) -> Result<Pool<T>, MemError> {
        let max = max.min(crate::stack::NONE - 1);
        let max_batches = max / BATCH as u32 + SPARE_BATCHES;
        Ok(Pool {
            slots: Region::reserve(max as usize * Self::SIZE, tag)?,
            max,
            carved: AtomicU32::new(0),
            slot_links: LinkTable::new(max, tag)?,
            stray: Stack::new(),
            batches: Region::reserve(max_batches as usize * mem::size_of::<Batch>(), tag)?,
            max_batches,
            batches_carved: AtomicU32::new(0),
            batch_links: LinkTable::new(max_batches, tag)?,
            full: Stack::new(),
            empty: Stack::new(),
            tag,
            _owns: PhantomData,
        })
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }

    pub fn max(&self) -> u32 {
        self.max
    }

    /// Slots made so far, live or free.
    pub fn carved(&self) -> u32 {
        self.carved.load(Ordering::Relaxed).min(self.max)
    }

    /// A worker's cache into this pool.
    pub fn cache(&self) -> Cache<'_, T> {
        Cache { pool: self, items: [0; CACHE], len: 0, stash: [0; STASH], stashed: 0 }
    }

    /// Batch descriptors made so far.
    #[cfg(test)]
    pub(crate) fn descriptors_made(&self) -> u32 {
        self.batches_carved.load(Ordering::Relaxed).min(self.max_batches)
    }

    fn slot(&self, index: u32) -> NonNull<T> {
        self.slots.at(index as usize * Self::SIZE).cast::<T>()
    }

    fn batch(&self, index: u32) -> NonNull<Batch> {
        self.batches.at(index as usize * mem::size_of::<Batch>()).cast::<Batch>()
    }

    /// Up to `BATCH` new slots, committed, their links made; their first index and count.
    fn carve(&self) -> Result<(u32, u32), MemError> {
        let start = self.carved.fetch_add(Self::CARVE, Ordering::Relaxed);
        if start >= self.max {
            self.carved.fetch_min(self.max, Ordering::Relaxed);
            return Err(MemError::Exhausted {
                tag: self.tag,
                reserved: self.slots.reserved(),
                wanted: (start as usize + 1) * Self::SIZE,
            });
        }
        let end = start.saturating_add(Self::CARVE).min(self.max);
        self.slots.ensure(end as usize * Self::SIZE)?;
        for index in start..end {
            // SAFETY: `[start, end)` came from the `fetch_add` just now: no other thread has
            // these indices.
            unsafe { self.slot_links.init(index) }?;
        }
        tag::allocated(self.tag, (end - start) as usize * Self::SIZE);
        Ok((start, end - start))
    }

    /// A new batch descriptor of the caller's own, when the empty stack has none.
    fn new_descriptor(&self) -> Result<u32, MemError> {
        let index = self.batches_carved.fetch_add(1, Ordering::Relaxed);
        if index >= self.max_batches {
            self.batches_carved.fetch_min(self.max_batches, Ordering::Relaxed);
            return Err(MemError::Exhausted {
                tag: self.tag,
                reserved: self.batches.reserved(),
                wanted: (index as usize + 1) * mem::size_of::<Batch>(),
            });
        }
        self.batches.ensure((index as usize + 1) * mem::size_of::<Batch>())?;
        // SAFETY: `index` came from the `fetch_add` just now: no other thread has it.
        unsafe { self.batch_links.init(index) }?;
        Ok(index)
    }

    /// Returns one slot, its value already moved out or dropped.
    fn give_one(&self, index: u32) {
        self.stray.push(index, &self.slot_links);
    }
}

impl<T> Drop for Pool<T> {
    fn drop(&mut self) {
        // Every `Pooled` borrowed the pool, so none is left: every slot is free and holds no
        // value. The regions unmap themselves.
    }
}

impl<T> fmt::Debug for Pool<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("tag", &self.tag)
            .field("max", &self.max)
            .field("carved", &self.carved())
            .finish()
    }
}

/// A worker's cache of free slots of a [`Pool`]. `Send`, not `Sync`: it belongs to one worker
/// at a time.
pub struct Cache<'p, T> {
    pool: &'p Pool<T>,
    items: [u32; CACHE],
    len: usize,
    /// Empty batch descriptors this cache holds: `stash[..stashed]`.
    stash: [u32; STASH],
    stashed: usize,
}

impl<'p, T> Cache<'p, T> {
    pub fn pool(&self) -> &'p Pool<T> {
        self.pool
    }

    /// Free slots it holds.
    pub fn cached(&self) -> usize {
        self.len
    }

    /// Places `value` in a slot of the pool.
    #[inline]
    pub fn try_alloc(&mut self, value: T) -> Result<Pooled<'p, T>, MemError> {
        if self.len == 0 {
            self.refill()?;
        }
        self.len -= 1;
        let index = self.items[self.len];
        let ptr = self.pool.slot(index);
        // SAFETY: a free slot is committed, aligned for `T`, holds no value and is held by
        // this cache alone; the `Pooled` made here becomes its one owner.
        unsafe { ptr.write(value) };
        Ok(Pooled { pool: self.pool, index, ptr })
    }

    /// Places `value` in a slot of the pool; stops when the pool is full.
    #[inline]
    #[track_caller]
    pub fn alloc(&mut self, value: T) -> Pooled<'p, T> {
        match self.try_alloc(value) {
            Ok(pooled) => pooled,
            Err(error) => fail(&error),
        }
    }

    /// Takes the value out and keeps its slot for the next allocation.
    #[inline]
    pub fn free(&mut self, pooled: Pooled<'p, T>) -> T {
        debug_assert!(std::ptr::eq(pooled.pool, self.pool), "a Pooled of another pool");
        let pooled = mem::ManuallyDrop::new(pooled);
        // SAFETY: the slot holds the value `pooled` owned, read out once; `pooled` is
        // forgotten, so it neither drops the value nor returns the slot.
        let value = unsafe { pooled.ptr.read() };
        self.keep(pooled.index);
        value
    }

    /// Drops the value and keeps its slot for the next allocation.
    #[inline]
    pub fn recycle(&mut self, pooled: Pooled<'p, T>) {
        drop(self.free(pooled));
    }

    #[inline]
    fn keep(&mut self, index: u32) {
        if self.len == CACHE {
            self.spill();
        }
        self.items[self.len] = index;
        self.len += 1;
    }

    #[cold]
    fn refill(&mut self) -> Result<(), MemError> {
        let pool = self.pool;
        if let Some(batch) = pool.full.pop(&pool.batch_links) {
            // SAFETY: the batch was popped, so this cache holds it alone; its pusher wrote it
            // before the release push this pop synchronized with.
            let items = unsafe { pool.batch(batch).as_ref() }.items;
            self.items[..BATCH].copy_from_slice(&items);
            self.len = BATCH;
            self.stash_descriptor(batch);
            return Ok(());
        }
        while self.len < BATCH {
            match pool.stray.pop(&pool.slot_links) {
                Some(index) => {
                    self.items[self.len] = index;
                    self.len += 1;
                }
                None => break,
            }
        }
        if self.len > 0 {
            return Ok(());
        }
        let (start, count) = pool.carve()?;
        for (slot, index) in self.items.iter_mut().zip(start..start + count) {
            *slot = index;
        }
        self.len = count as usize;
        Ok(())
    }

    /// Keeps the empty descriptor `batch`; a full stash first gives [`MOVE`] of its own back
    /// to the pool's empty stack, with one compare-and-swap.
    fn stash_descriptor(&mut self, batch: u32) {
        if self.stashed == STASH {
            let pool = self.pool;
            pool.empty.push_all(&self.stash[STASH - MOVE..], &pool.batch_links);
            self.stashed = STASH - MOVE;
        }
        self.stash[self.stashed] = batch;
        self.stashed += 1;
    }

    /// An empty descriptor of this cache's own: from its stash, refilled with up to [`MOVE`]
    /// from the pool's empty stack in one compare-and-swap, else a new one.
    fn take_descriptor(&mut self) -> Result<u32, MemError> {
        if self.stashed == 0 {
            let pool = self.pool;
            self.stashed = pool.empty.pop_some(&pool.batch_links, &mut self.stash[..MOVE]);
            if self.stashed == 0 {
                return pool.new_descriptor();
            }
        }
        self.stashed -= 1;
        Ok(self.stash[self.stashed])
    }

    #[cold]
    fn spill(&mut self) {
        let pool = self.pool;
        match self.take_descriptor() {
            Ok(batch) => {
                let mut items = [0; BATCH];
                items.copy_from_slice(&self.items[self.len - BATCH..self.len]);
                // SAFETY: the descriptor is this cache's alone (popped or carved), committed
                // and aligned; the push below publishes it with a release store.
                unsafe { pool.batch(batch).write(Batch { items }) };
                pool.full.push(batch, &pool.batch_links);
            }
            // Every descriptor is in use: the slots still go back, one at a time.
            Err(_) => {
                for &index in &self.items[self.len - BATCH..self.len] {
                    pool.give_one(index);
                }
            }
        }
        self.len -= BATCH;
    }
}

impl<T> Drop for Cache<'_, T> {
    fn drop(&mut self) {
        while self.len >= BATCH {
            self.spill();
        }
        for &index in &self.items[..self.len] {
            self.pool.give_one(index);
        }
        self.len = 0;
        self.pool.empty.push_all(&self.stash[..self.stashed], &self.pool.batch_links);
        self.stashed = 0;
    }
}

// SAFETY: a cache holds free indices only, no value; moving it moves which thread returns them.
unsafe impl<T: Send> Send for Cache<'_, T> {}

/// A `T` in a slot of a [`Pool`], owned like a `Box<T>`. Dropping it drops the value and
/// returns the slot to the pool (one compare-and-swap); [`Cache::free`] returns it to a cache
/// instead, which batches.
pub struct Pooled<'p, T> {
    pool: &'p Pool<T>,
    index: u32,
    ptr: NonNull<T>,
}

// SAFETY: a `Pooled` owns its value as a `Box` does.
unsafe impl<T: Send> Send for Pooled<'_, T> {}
// SAFETY: `&Pooled<T>` gives only `&T`.
unsafe impl<T: Sync> Sync for Pooled<'_, T> {}

impl<'p, T> Pooled<'p, T> {
    /// The slot's index: a `u32` ID, stable while this owns it.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// The value's address, its slot kept: for a table that publishes it by pointer.
    /// [`Pooled::from_raw`] takes it back.
    pub fn into_raw(self) -> NonNull<T> {
        mem::ManuallyDrop::new(self).ptr
    }

    /// The `Pooled` [`Pooled::into_raw`] gave `ptr` for.
    ///
    /// # Safety
    ///
    /// `ptr` came from `into_raw` of a `Pooled` of `pool`, and is taken back once.
    pub unsafe fn from_raw(pool: &'p Pool<T>, ptr: NonNull<T>) -> Pooled<'p, T> {
        let offset = (ptr.as_ptr() as usize).wrapping_sub(pool.slots.base().as_ptr() as usize);
        let index = offset / Pool::<T>::SIZE;
        debug_assert!(
            offset % Pool::<T>::SIZE == 0 && index < pool.max as usize,
            "a pointer into another pool"
        );
        Pooled { pool, index: index as u32, ptr }
    }

    /// Takes the value out; the slot goes back to the pool.
    pub fn into_inner(self) -> T {
        let this = mem::ManuallyDrop::new(self);
        // SAFETY: the slot holds the value this owned, read out once; `this` is forgotten.
        let value = unsafe { this.ptr.read() };
        this.pool.give_one(this.index);
        value
    }
}

impl<T> Deref for Pooled<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the slot holds this value for as long as `self` lives.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for Pooled<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`, and `&mut self` makes the borrow unique.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> Drop for Pooled<'_, T> {
    fn drop(&mut self) {
        // SAFETY: the slot holds this value, dropped once; the slot is returned after.
        unsafe { self.ptr.drop_in_place() };
        self.pool.give_one(self.index);
    }
}

impl<T: fmt::Debug> fmt::Debug for Pooled<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// The slot of a [`Shared`] value: the value and the count of its owners. A pool of shared
/// values is a `Pool<Counted<T>>`, and its caches make them with [`Cache::share`].
pub struct Counted<T> {
    owners: AtomicU32,
    value: T,
}

/// Owners past this abort, as `Arc` does: a count that wraps would free a value still held.
const MAX_OWNERS: u32 = i32::MAX as u32;

impl<'p, T> Cache<'p, Counted<T>> {
    /// Places `value` in a slot of the pool with one owner.
    #[inline]
    pub fn try_share(&mut self, value: T) -> Result<Shared<'p, T>, MemError> {
        let pooled = self.try_alloc(Counted { owners: AtomicU32::new(1), value })?;
        let pooled = mem::ManuallyDrop::new(pooled);
        Ok(Shared { pool: pooled.pool, index: pooled.index, ptr: pooled.ptr })
    }

    /// Places `value` in a slot of the pool with one owner; stops when the pool is full.
    #[inline]
    #[track_caller]
    pub fn share(&mut self, value: T) -> Shared<'p, T> {
        match self.try_share(value) {
            Ok(shared) => shared,
            Err(error) => fail(&error),
        }
    }
}

impl<'p, E: Clone, const N: usize> Cache<'p, Counted<[E; N]>> {
    /// Places `head`, then clones of `fill` up to `N` elements, in a slot with one owner. Each
    /// element is written in place: the array never passes through the stack, so a slot of
    /// megabytes works, and a copy (`head` a whole array) is one pass over the slot.
    pub fn try_share_from(
        &mut self,
        head: &[E],
        fill: &E,
    ) -> Result<Shared<'p, [E; N]>, MemError> {
        assert!(head.len() <= N, "{} elements for a slot of {N}", head.len());
        if self.len == 0 {
            self.refill()?;
        }
        self.len -= 1;
        let index = self.items[self.len];
        let ptr = self.pool.slot(index);
        let slot = ptr.as_ptr();
        // SAFETY: a free slot is committed, aligned for `Counted<[E; N]>`, holds no value and is
        // held by this cache alone; its fields are written through raw pointers, never read.
        let owners = unsafe { &raw mut (*slot).owners };
        // SAFETY: as above: the slot's count, written once, before anything reads it.
        unsafe { owners.write(AtomicU32::new(1)) };
        // SAFETY: as above: a place of the slot, taken without reading it.
        let items = unsafe { &raw mut (*slot).value }.cast::<E>();
        let mut written = Written { pool: self.pool, index, items, len: 0 };
        for item in head {
            // SAFETY: `written.len < head.len() <= N`: in the array, and nothing is there yet.
            unsafe { items.wrapping_add(written.len).write(item.clone()) }; // clone: the copy into the slot is this call's work
            written.len += 1;
        }
        while written.len < N {
            // SAFETY: as above, `written.len < N`.
            unsafe { items.wrapping_add(written.len).write(fill.clone()) }; // clone: the copy into the slot is this call's work
            written.len += 1;
        }
        mem::forget(written);
        Ok(Shared { pool: self.pool, index, ptr })
    }

    /// As [`Cache::try_share_from`]; stops when the pool is full.
    #[inline]
    #[track_caller]
    pub fn share_from(&mut self, head: &[E], fill: &E) -> Shared<'p, [E; N]> {
        match self.try_share_from(head, fill) {
            Ok(shared) => shared,
            Err(error) => fail(&error),
        }
    }
}

/// The elements a `try_share_from` has written: dropped, and the slot returned, if a clone
/// panics.
struct Written<'p, E, T> {
    pool: &'p Pool<T>,
    index: u32,
    items: *mut E,
    len: usize,
}

impl<E, T> Drop for Written<'_, E, T> {
    fn drop(&mut self) {
        // SAFETY: the first `len` elements were written and nothing else owns them; the slot's
        // count is a plain atomic with nothing to drop.
        unsafe { ptr::slice_from_raw_parts_mut(self.items, self.len).drop_in_place() };
        self.pool.give_one(self.index);
    }
}

/// A `T` in a slot of a [`Pool`], owned like an `Arc<T>`: [`Clone`] adds an owner (one atomic
/// add), the last owner's drop drops the value and returns the slot, and [`Shared::make_mut`]
/// copies into a fresh slot only while the value is shared. For copy-on-write chunks that come
/// and go at a steady rate: their slots are reused, never freed to the global allocator.
pub struct Shared<'p, T> {
    pool: &'p Pool<Counted<T>>,
    index: u32,
    ptr: NonNull<Counted<T>>,
}

// SAFETY: as for `Arc`: owners on several threads read the value and the last one drops it.
unsafe impl<T: Send + Sync> Send for Shared<'_, T> {}
// SAFETY: as for `Arc`: `&Shared<T>` gives `&T` and can make another owner.
unsafe impl<T: Send + Sync> Sync for Shared<'_, T> {}

impl<'p, T> Shared<'p, T> {
    fn counted(&self) -> &Counted<T> {
        // SAFETY: the slot holds a value while any owner, such as `self`, lives.
        unsafe { self.ptr.as_ref() }
    }

    /// The slot's index: a `u32` ID, stable while any owner lives.
    pub fn index(this: &Self) -> u32 {
        this.index
    }

    /// How many owners the value has now.
    pub fn owners(this: &Self) -> u32 {
        this.counted().owners.load(Ordering::Acquire)
    }

    /// Whether both are owners of the same slot.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.ptr == other.ptr
    }

    /// The value to change in place, if `this` is its one owner.
    pub fn get_mut(this: &mut Self) -> Option<&mut T> {
        if Shared::owners(this) != 1 {
            return None;
        }
        // SAFETY: `this` is the one owner and is borrowed uniquely, so no other `Shared` of the
        // slot exists and none can be made meanwhile (only cloning `this` makes one). The
        // acquire load ordered every former owner's reads before this write.
        Some(unsafe { &mut (*this.ptr.as_ptr()).value })
    }

    /// The value to change in place: first copied into a fresh slot from `cache` while other
    /// owners share it, which then keep the old one.
    #[inline]
    #[track_caller]
    pub fn make_mut<'s>(this: &'s mut Self, cache: &mut Cache<'p, Counted<T>>) -> &'s mut T
    where
        T: Clone,
    {
        if Shared::owners(this) != 1 {
            *this = cache.share(T::clone(this));
        }
        match Shared::get_mut(this) {
            Some(value) => value,
            None => unreachable!("a value just placed has one owner"),
        }
    }
}

impl<T> Clone for Shared<'_, T> {
    fn clone(&self) -> Self {
        let before = self.counted().owners.fetch_add(1, Ordering::Relaxed);
        if before >= MAX_OWNERS {
            std::process::abort();
        }
        Shared { pool: self.pool, index: self.index, ptr: self.ptr }
    }
}

impl<T> Deref for Shared<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.counted().value
    }
}

impl<T> Drop for Shared<'_, T> {
    fn drop(&mut self) {
        if self.counted().owners.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        // SAFETY: the count reached zero here, so this was the last owner, and the acquire half
        // ordered every other owner's reads before it: the value is dropped once, then the
        // slot returned.
        unsafe { self.ptr.drop_in_place() };
        self.pool.give_one(self.index);
    }
}

impl<T: fmt::Debug> fmt::Debug for Shared<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

#[derive(Clone, Copy)]
struct Retired {
    grace: Grace,
    index: u32,
}

/// Objects of one pool that were unlinked while readers may still hold them, each kept in its
/// slot until its grace has passed (docs/v2/plan/design/concurrency.md "Task boundaries
/// reclaim memory"), then dropped and its slot returned through the limbo's own cache. For
/// one owner (a writer of the table they were unlinked from).
///
/// Graces are held in start order, so the passed ones are a prefix and [`Limbo::collect`]
/// finds it with a binary search. Its list sits in a reserved column as long as the pool, so
/// it never allocates; a drained prefix is reused in place.
pub struct Limbo<'p, T> {
    cache: Cache<'p, T>,
    held: RegionVec<Retired>,
    /// Entries before it were collected.
    head: usize,
}

impl<'p, T> Limbo<'p, T> {
    /// A limbo for `pool`, with room for every slot it can make.
    pub fn new(pool: &'p Pool<T>) -> Result<Limbo<'p, T>, MemError> {
        Ok(Limbo {
            cache: pool.cache(),
            held: RegionVec::new(pool.max() as usize, pool.tag())?,
            head: 0,
        })
    }

    pub fn pool(&self) -> &'p Pool<T> {
        self.cache.pool
    }

    /// Objects held, not yet collected.
    pub fn len(&self) -> usize {
        self.held.len() - self.head
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Retires `pooled`, which the caller has unlinked (its release stores done): it is
    /// dropped once every task that may have read it has ended.
    pub fn retire(&mut self, pooled: Pooled<'p, T>) {
        self.hold(Grace::start(), pooled);
    }

    /// Retires `pooled` under a grace the caller started after unlinking it (one grace for a
    /// batch). A grace older than one held already is held as the newer one: later, never
    /// sooner.
    pub fn hold(&mut self, grace: Grace, pooled: Pooled<'p, T>) {
        assert!(std::ptr::eq(pooled.pool, self.cache.pool), "a Pooled of another pool");
        let pooled = mem::ManuallyDrop::new(pooled);
        let grace = match self.held.last() {
            Some(last) if self.head < self.held.len() => grace.max(last.grace),
            _ => grace,
        };
        if self.head == self.held.len() {
            self.held.clear();
            self.head = 0;
        } else if self.head > 0
            && (self.head * 2 >= self.held.len() || self.held.len() == self.held.max_len())
        {
            let live = self.held.len() - self.head;
            self.held.copy_within(self.head.., 0);
            self.held.truncate(live);
            self.head = 0;
        }
        // Each entry owns a distinct slot and `pooled` owns one more, so the entries fit in a
        // column as long as the pool.
        self.held.push(Retired { grace, index: pooled.index });
    }

    /// Drops every object whose grace has passed and keeps its slot for the pool. Returns how
    /// many it freed.
    pub fn collect(&mut self) -> usize {
        let passed = self.held[self.head..].partition_point(|retired| retired.grace.passed());
        for at in self.head..self.head + passed {
            let index = self.held[at].index;
            // SAFETY: the slot holds the value its `Pooled` owned when it was retired (the
            // `Pooled` was forgotten), no reader can reach it once its grace has passed, and the
            // head moves past it below, so it is dropped once.
            unsafe { self.cache.pool.slot(index).drop_in_place() };
            self.cache.keep(index);
        }
        self.head += passed;
        passed
    }
}

impl<T> Drop for Limbo<'_, T> {
    fn drop(&mut self) {
        // As `blaze_reclaim::Limbo`: its owner drops it only once no reader can reach what it
        // holds, so everything goes at once.
        for at in self.head..self.held.len() {
            let index = self.held[at].index;
            // SAFETY: the slot holds the value retired into it, dropped once: the limbo is
            // going away and its owner says no reader is left.
            unsafe { self.cache.pool.slot(index).drop_in_place() };
            self.cache.keep(index);
        }
        self.head = self.held.len();
    }
}

impl<T> fmt::Debug for Limbo<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Limbo")
            .field("tag", &self.cache.pool.tag)
            .field("held", &self.len())
            .finish()
    }
}
