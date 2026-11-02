//! The request arena (docs/v2/plan/design/memory.md, "a request"): memory for everything one
//! edit, build or test run makes and drops together, freed at once when the request ends.
//!
//! An [`Arena`] is shared by the request's tasks on every worker. It holds 2 MB chunks from the
//! process's cache ([`chunks`](crate::chunks)) and hands them out in 64 KB blocks; each task
//! allocates through an [`ArenaLocal`] of its own, a bump pointer in its current block, so the
//! arena's shared word is written once per 64 KB, not once per allocation. Values live as long
//! as the arena (`&'a`), however short the `ArenaLocal` that made them. An allocation bigger
//! than a block takes blocks of its own; one bigger than a chunk gets a region of its own.
//!
//! Dropping or resetting the arena gives its chunks back to the cache, still committed, so the
//! next request starts warm: no system call and no global allocation in the warm loop.
//!
//! Cost: a small allocation is a bump in the local block (about 2 ns); a block refill is one
//! compare-and-swap on the arena; a chunk refill is one pop from the cache.

use std::{
    alloc::{AllocError, Allocator, Layout},
    cell::Cell,
    mem::{ManuallyDrop, MaybeUninit},
    ptr::{self, NonNull},
    slice, str,
    sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering},
};

use crate::{
    MemError, Region, Tag,
    chunks::{self, CHUNK, Chunk},
    error::fail,
    stack::{Links, NONE, Stack},
    tag,
};

/// The unit an [`ArenaLocal`] takes from its arena.
pub const BLOCK: usize = 64 << 10;

const BLOCKS: u32 = (CHUNK / BLOCK) as u32;

/// An allocation of at most this goes in the local block; a bigger one takes blocks of its
/// own, so a block's unused tail is at most this.
const SMALL: usize = BLOCK / 8;

/// `current`'s empty state: no chunk.
const NO_CHUNK: u64 = pack(NONE, BLOCKS);

const fn pack(chunk: u32, next_block: u32) -> u64 {
    ((chunk as u64) << 32) | next_block as u64
}

/// The chunks' own link table, so an arena chains the chunks it holds without memory of its
/// own.
struct ChunkLinks;

impl Links for ChunkLinks {
    fn link(&self, index: u32) -> &crate::sync::atomic::AtomicU32 {
        chunks::link(index)
    }
}

/// A region of one allocation bigger than a chunk, its header at its start.
struct Large {
    next: *mut Large,
    region: ManuallyDrop<Region>,
}

/// Memory for one request, shared by its tasks. Allocate through [`Arena::local`].
pub struct Arena {
    tag: Tag,
    /// The chunk blocks are handed out of and its next block, packed.
    current: AtomicU64,
    /// Every chunk the arena holds, linked through the chunks' link table.
    held: Stack,
    /// Regions of allocations bigger than a chunk.
    large: AtomicPtr<Large>,
    held_bytes: AtomicUsize,
    allocated: AtomicUsize,
}

// SAFETY: every shared field is atomic; the chunks and regions it holds are reached only
// through blocks handed out once each, whose users synchronize their own data.
unsafe impl Sync for Arena {}
// SAFETY: as for `Sync`; the `*mut Large` it holds are owned regions, freed by whoever drops it.
unsafe impl Send for Arena {}

impl Arena {
    /// An empty arena for `tag`: no memory, no system call until its first allocation.
    pub const fn new(tag: Tag) -> Arena {
        Arena {
            tag,
            current: AtomicU64::new(NO_CHUNK),
            held: Stack::new(),
            large: AtomicPtr::new(ptr::null_mut()),
            held_bytes: AtomicUsize::new(0),
            allocated: AtomicUsize::new(0),
        }
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }

    /// A bump allocator into this arena, for one task or worker.
    pub fn local(&self) -> ArenaLocal<'_> {
        ArenaLocal { arena: self, block: Cell::new(ptr::null_mut()), used: Cell::new(0) }
    }

    /// Bytes of chunks and regions it holds.
    pub fn held(&self) -> usize {
        self.held_bytes.load(Ordering::Relaxed)
    }

    /// Bytes its locals took in blocks and large regions.
    pub fn allocated(&self) -> usize {
        self.allocated.load(Ordering::Relaxed)
    }

    /// `n` contiguous blocks (`n <= BLOCKS`): from the current chunk if it has room, else
    /// from a chunk taken for them, which becomes current.
    fn blocks(&self, n: u32) -> Result<NonNull<u8>, MemError> {
        debug_assert!((1..=BLOCKS).contains(&n));
        let mut current = self.current.load(Ordering::Acquire);
        loop {
            let chunk = (current >> 32) as u32;
            let next = current as u32;
            if chunk != NONE && next + n <= BLOCKS {
                match self.current.compare_exchange_weak(
                    current,
                    pack(chunk, next + n),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        self.taken(n as usize * BLOCK);
                        let base = Chunk::from_index(chunk).ptr();
                        return Ok(offset(base, next as usize * BLOCK));
                    }
                    Err(now) => {
                        current = now;
                        continue;
                    }
                }
            }
            let fresh = chunks::take(self.tag)?;
            match self.current.compare_exchange(
                current,
                pack(fresh.index(), n),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.held.push(fresh.index(), &ChunkLinks);
                    self.held_bytes.fetch_add(CHUNK, Ordering::Relaxed);
                    self.taken(n as usize * BLOCK);
                    return Ok(fresh.ptr());
                }
                Err(now) => {
                    // Another local installed a chunk first: use its room, give this one back.
                    chunks::give(fresh, self.tag);
                    current = now;
                }
            }
        }
    }

    fn taken(&self, bytes: usize) {
        self.allocated.fetch_add(bytes, Ordering::Relaxed);
        tag::allocated(self.tag, bytes);
    }

    /// A region of its own for an allocation bigger than a chunk.
    #[cold]
    fn large(&self, layout: Layout) -> Result<NonNull<u8>, MemError> {
        if layout.align() > crate::os::RESERVE_ALIGN {
            return Err(MemError::TooLarge {
                tag: self.tag,
                what: "an arena allocation's alignment",
                wanted: layout.align(),
            });
        }
        let header = Layout::new::<Large>();
        let start = header.size().next_multiple_of(layout.align().max(header.align()));
        let bytes = start.checked_add(layout.size()).ok_or(MemError::TooLarge {
            tag: self.tag,
            what: "an arena allocation",
            wanted: layout.size(),
        })?;
        let region = Region::reserve_huge(bytes, self.tag)?;
        region.ensure(bytes)?;
        let base = region.base();
        let node = base.cast::<Large>();
        let reserved = region.reserved();
        // SAFETY: the region is fresh, committed for `bytes >= size_of::<Large>()` and
        // page-aligned, so its start holds a `Large`; nothing else knows of it yet.
        unsafe {
            node.write(Large { next: ptr::null_mut(), region: ManuallyDrop::new(region) })
        };
        let mut head = self.large.load(Ordering::Relaxed);
        loop {
            // SAFETY: `node` is ours until the swap below publishes it.
            unsafe { (*node.as_ptr()).next = head };
            match self.large.compare_exchange_weak(
                head,
                node.as_ptr(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(now) => head = now,
            }
        }
        self.held_bytes.fetch_add(reserved, Ordering::Relaxed);
        self.taken(layout.size());
        Ok(offset(base, start))
    }

    /// Frees everything: its chunks go back to the cache, its large regions to the kernel.
    pub fn reset(&mut self) {
        // `&mut self`: no local takes a chunk meanwhile, so the held list is walked with plain
        // loads and given back up to `GIVE_RUN` chunks per compare-and-swap.
        let mut run = [NONE; chunks::GIVE_RUN];
        let mut len = 0;
        let mut next = self.held.take_all();
        while next != NONE {
            run[len] = next;
            len += 1;
            next = ChunkLinks.link(next).load(Ordering::Relaxed);
            if len == run.len() {
                chunks::give_all(&run, self.tag);
                len = 0;
            }
        }
        chunks::give_all(&run[..len], self.tag);
        let mut node = *self.large.get_mut();
        *self.large.get_mut() = ptr::null_mut();
        while !node.is_null() {
            // SAFETY: `node` heads a region this arena made in `large` and linked once; `&mut
            // self` says no block of it is borrowed, so this is the only reference to it.
            let header = unsafe { &mut *node };
            let next = header.next;
            // SAFETY: the region is taken out of its own header once; the header is not used
            // after, as dropping the region unmaps it.
            let region = unsafe { ManuallyDrop::take(&mut header.region) };
            drop(region);
            node = next;
        }
        *self.current.get_mut() = NO_CHUNK;
        *self.held_bytes.get_mut() = 0;
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        self.reset();
    }
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena")
            .field("tag", &self.tag)
            .field("held", &self.held())
            .field("allocated", &self.allocated())
            .finish()
    }
}

fn offset(base: NonNull<u8>, by: usize) -> NonNull<u8> {
    // SAFETY: callers offset within a block, chunk or region they hold, which does not wrap
    // around the address space, so the result is not null.
    unsafe { NonNull::new_unchecked(base.as_ptr().wrapping_add(by)) }
}

/// One task's bump allocator into an [`Arena`]. What it allocates lives as long as the arena.
/// Not `Sync`; it may move to another thread with its task.
pub struct ArenaLocal<'a> {
    arena: &'a Arena,
    /// The block it bumps in (null: none yet) and the bytes of it used.
    block: Cell<*mut u8>,
    used: Cell<usize>,
}

// SAFETY: the block it bumps in is its own (handed out once by the arena), and the arena is
// `Sync`; moving the local moves that ownership.
unsafe impl Send for ArenaLocal<'_> {}

impl<'a> ArenaLocal<'a> {
    pub fn arena(&self) -> &'a Arena {
        self.arena
    }

    /// Takes `layout`'s bytes, aligned, for as long as the arena lives.
    #[inline]
    pub fn try_alloc_layout(&self, layout: Layout) -> Result<NonNull<u8>, MemError> {
        if let Some(block) = NonNull::new(self.block.get()) {
            let at = block.as_ptr() as usize + self.used.get();
            let from = crate::align::align_up(at, layout.align()) - block.as_ptr() as usize;
            let end = from + layout.size();
            if end <= BLOCK {
                self.used.set(end);
                return Ok(offset(block, from));
            }
        }
        self.refill(layout)
    }

    #[cold]
    fn refill(&self, layout: Layout) -> Result<NonNull<u8>, MemError> {
        if layout.size() > SMALL || layout.align() > SMALL {
            return self.alone(layout);
        }
        let block = self.arena.blocks(1)?;
        // Blocks are aligned to `BLOCK`, so any alignment up to `SMALL` holds at their start.
        self.block.set(block.as_ptr());
        self.used.set(layout.size());
        Ok(block)
    }

    /// Blocks (or a region) of its own for an allocation too big for the local block.
    fn alone(&self, layout: Layout) -> Result<NonNull<u8>, MemError> {
        if layout.align() <= BLOCK && layout.size() <= CHUNK {
            let n = layout.size().div_ceil(BLOCK).max(1) as u32;
            return self.arena.blocks(n);
        }
        self.arena.large(layout)
    }

    #[inline]
    #[track_caller]
    pub fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
        match self.try_alloc_layout(layout) {
            Ok(ptr) => ptr,
            Err(error) => fail(&error),
        }
    }

    /// Places `value` in the arena. Its destructor never runs: the arena forgets it at reset.
    #[inline]
    #[track_caller]
    pub fn alloc<T>(&self, value: T) -> &'a mut T {
        let ptr = self.alloc_layout(Layout::new::<T>()).cast::<T>();
        // SAFETY: fresh memory of the arena, sized and aligned for `T`, handed out once; it is
        // valid until the arena resets, which takes `&mut Arena` and so outlives `'a`.
        unsafe { ptr.write(value) };
        // SAFETY: just initialized, and no other reference to it exists.
        unsafe { &mut *ptr.as_ptr() }
    }

    #[inline]
    #[track_caller]
    pub fn alloc_uninit_slice<T>(&self, len: usize) -> &'a mut [MaybeUninit<T>] {
        let layout = match Layout::array::<T>(len) {
            Ok(layout) => layout,
            Err(_) => fail(&MemError::TooLarge {
                tag: self.arena.tag,
                what: "an arena slice",
                wanted: len,
            }),
        };
        let ptr = self.alloc_layout(layout).cast::<MaybeUninit<T>>();
        // SAFETY: fresh memory for `len` elements, aligned, handed out once, valid for `'a`.
        unsafe { slice::from_raw_parts_mut(ptr.as_ptr(), len) }
    }

    #[inline]
    #[track_caller]
    pub fn alloc_slice_copy<T: Copy>(&self, values: &[T]) -> &'a mut [T] {
        let slots = self.alloc_uninit_slice::<T>(values.len());
        for (slot, value) in slots.iter_mut().zip(values) {
            slot.write(*value);
        }
        // SAFETY: every element was just written.
        unsafe { slots.assume_init_mut() }
    }

    #[inline]
    #[track_caller]
    pub fn alloc_slice_fill_with<T>(&self, len: usize, mut f: impl FnMut(usize) -> T) -> &'a mut [T] {
        let slots = self.alloc_uninit_slice::<T>(len);
        for (i, slot) in slots.iter_mut().enumerate() {
            slot.write(f(i));
        }
        // SAFETY: every element was just written.
        unsafe { slots.assume_init_mut() }
    }

    #[inline]
    #[track_caller]
    pub fn alloc_str(&self, text: &str) -> &'a mut str {
        let bytes = self.alloc_slice_copy(text.as_bytes());
        // SAFETY: a copy of a `str`'s bytes, so UTF-8.
        unsafe { str::from_utf8_unchecked_mut(bytes) }
    }

    /// Keeps what `vec` holds for the arena's life, as a slice: a vector built in the arena
    /// and then frozen.
    pub fn keep<T>(&self, vec: Vec<T, &ArenaLocal<'a>>) -> &'a mut [T] {
        let mut vec = ManuallyDrop::new(vec);
        let len = vec.len();
        let ptr = vec.as_mut_ptr();
        // SAFETY: the vector's buffer is a block of an arena that lives for `'a` (its
        // allocator is an `ArenaLocal<'a>`, which borrows its arena for `'a`), its first `len`
        // elements are initialized, and the vector is forgotten, so nothing frees the buffer
        // or drops the elements.
        unsafe { slice::from_raw_parts_mut(ptr, len) }
    }

    /// Whether `[ptr, ptr + size)` is in the local block and ends where its free part begins.
    fn is_top(&self, ptr: NonNull<u8>, size: usize) -> bool {
        let block = self.block.get() as usize;
        let at = ptr.as_ptr() as usize;
        block != 0 && at >= block && at + size == block + self.used.get()
    }

    /// The offset of `ptr`, the top allocation, in the local block.
    fn offset_in_block(&self, ptr: NonNull<u8>) -> usize {
        ptr.as_ptr() as usize - self.block.get() as usize
    }
}

// SAFETY: blocks come from the local's own block or from blocks and regions the arena hands
// out once each, disjoint and committed. Each stays valid until the arena resets or drops, both
// of which take `&mut Arena` or ownership, impossible while this local (which borrows the arena
// for `'a`) is alive. `deallocate` and `shrink` move only this local's cursor, and only over the
// top block; nothing here unwinds.
unsafe impl Allocator for ArenaLocal<'_> {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let ptr = self.try_alloc_layout(layout).map_err(|_| AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if self.is_top(ptr, layout.size()) {
            self.used.set(self.offset_in_block(ptr));
        }
    }

    #[inline]
    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if self.is_top(ptr, old.size())
            && ptr.as_ptr().align_offset(new.align()) == 0
            && self.offset_in_block(ptr) + new.size() <= BLOCK
        {
            self.used.set(self.offset_in_block(ptr) + new.size());
            return Ok(NonNull::slice_from_raw_parts(ptr, new.size()));
        }
        let to = self.try_alloc_layout(new).map_err(|_| AllocError)?;
        // SAFETY: the caller says `ptr` is a live block of `old.size()` bytes; `to` is fresh
        // and at least as big, so they do not overlap.
        unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), to.as_ptr(), old.size()) };
        Ok(NonNull::slice_from_raw_parts(to, new.size()))
    }

    #[inline]
    unsafe fn grow_zeroed(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's guarantees for `grow_zeroed` are those of `grow`.
        let block = unsafe { self.grow(ptr, old, new) }?;
        let tail = block.cast::<u8>().as_ptr().wrapping_add(old.size());
        // SAFETY: `[old.size(), new.size())` is inside the block; chunks are reused, so the
        // bytes are zeroed here rather than assumed.
        unsafe { ptr::write_bytes(tail, 0, new.size() - old.size()) };
        Ok(block)
    }

    #[inline]
    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if ptr.as_ptr().align_offset(new.align()) != 0 {
            let to = self.try_alloc_layout(new).map_err(|_| AllocError)?;
            // SAFETY: `ptr` is a live block of at least `new.size()` bytes, `to` a fresh one.
            unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), to.as_ptr(), new.size()) };
            return Ok(NonNull::slice_from_raw_parts(to, new.size()));
        }
        if self.is_top(ptr, old.size()) {
            self.used.set(self.offset_in_block(ptr) + new.size());
        }
        Ok(NonNull::slice_from_raw_parts(ptr, new.size()))
    }
}
