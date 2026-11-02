//! The frame allocator: a bump pointer over a chain of blocks, one frame per task, reset when
//! the task that used it ends (docs/v2/plan/design/memory.md, "a task").
//!
//! [`task`] gives the closure a frame of its own, empty, and frees it when the closure returns
//! (or unwinds); a task inside a task gets another frame, so an inner end never frees what the
//! outer task holds, at any depth. Within a task, [`Frame::scope`] and [`Marker`]s roll back to
//! a point. A frame implements [`Allocator`], so `Vec<T, &Frame>`, `Box<T, &Frame>` and the
//! rest allocate from it.
//!
//! A frame's memory is a chain of blocks, each with a header at its start: 2 MB chunks from
//! the process's cache ([`chunks`]), and for an allocation over half a chunk a span, a region
//! of its own reserved far past it, so the top allocation grows in place there however big it
//! gets. A task's frame is a header on [`task`]'s stack. An inner task's starts in the rest of
//! its enclosing task's chunk, which the enclosing frame leaves alone until the inner task
//! ends (it takes another block if it allocates meanwhile), so deep nesting of small tasks
//! shares one chunk. Past that its chunks come from the thread's shelf ([`SHELF`] chunks, no
//! atomic), then the process's cache (one compare-and-swap), and go back the same way; its
//! spans come from and go back to the thread's shelf of spans, their pages committed. So nested
//! tasks to any depth make no system call and no global allocation once warm. A frame made with
//! [`Frame::new`] keeps its blocks across [`Frame::reset`].
//!
//! Resetting is a few stores: nothing is freed piecemeal, no destructor of a value placed with
//! [`Frame::alloc`] runs (collections in a frame drop their elements as usual); [`Frame::trim`]
//! and [`trim_spare`] give pages back after a burst.
//!
//! A task's frame grows under the process's [`Cap`] when one is set ([`set_cap`]): past the
//! chunks its thread keeps warm, each chunk and each span's pages are reserved from it first, a
//! reserve over the limit waits, and when every holder waits the newest is cancelled: its task
//! unwinds with [`Cancelled`] (from inside the allocation that waited) and is run again.
//!
//! Chunks are placed on the calling thread's NUMA node ([`chunks::set_thread_node`]), and so
//! are spans.
//!
//! [`measure`] says the most the calling thread's frames held at once while a closure ran, a
//! nested task's on top of its enclosing task's, counted at task and measure boundaries only.
//!
//! Cost: an allocation is an align, an add, a compare against the block's end and one store
//! of the cursor (no atomic, no shared line, and no peak: that is folded in where the cursor
//! moves back); about 2 ns. Freeing or growing the most recent
//! allocation happens in place, so a `Vec<T, &Frame>` pushed in a loop never copies while
//! nothing else is allocated after it and it stays in its block (in a span, always).

mod cap;

pub use cap::{Cancelled, Cap, Grant, Spills, cap, set_cap};

use std::{
    alloc::{AllocError, Allocator, Layout},
    cell::Cell,
    mem::{self, ManuallyDrop, MaybeUninit},
    panic::Location,
    ptr::{self, NonNull},
    slice, str,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::{
    MemError, Region, Tag,
    align::align_up,
    chunks::{self, CHUNK, Chunk},
    error::fail,
    tag,
};

/// The most one frame holds unless made with [`Frame::with_reserve`]: what one task may take.
#[cfg(target_pointer_width = "64")]
pub const FRAME_RESERVE: usize = 4 << 30;

/// The most one frame holds on a 32-bit target: 16 MB. There (wasm32) a reservation is memory
/// taken whole, and the whole address space is 4 GB.
#[cfg(not(target_pointer_width = "64"))]
pub const FRAME_RESERVE: usize = 16 << 20;

/// Chunks a thread keeps warm for its next tasks, outside the [`Cap`]: 16 MB.
pub const SHELF: usize = 8;

/// Spans a thread keeps warm, their pages committed.
const SPANS: usize = 64;

/// An allocation bigger than this goes in a span rather than a chunk.
const SPAN_AT: usize = CHUNK / 2;

/// The largest alignment a frame gives.
const MAX_ALIGN: usize = 1 << 30;

/// Where an empty frame's cursor and end sit: not null, in no block.
const EMPTY: usize = 4096;

/// The least rest of a chunk an enclosing task's frame lends an inner task's.
const LEND_AT: usize = 4096;

static IDS: AtomicU32 = AtomicU32::new(0);

/// A block's header, at its first byte; its data follows.
#[repr(C, align(64))]
struct Block {
    prev: *mut Block,
    next: *mut Block,
    /// The bytes the frame used in the blocks before this one, when it became current.
    before: usize,
    /// One past the last byte it can hold: the chunk's end, or its span's reservation's.
    end: usize,
    /// The chunk it is, or `stack::NONE` for a span.
    chunk: u32,
    /// The span's bytes reserved from the cap.
    granted: usize,
    /// The span's region, which this header is in; none for a chunk.
    span: Option<ManuallyDrop<Region>>,
}

const HEADER: usize = mem::size_of::<Block>();

const _: () = assert!(HEADER.is_multiple_of(64) && HEADER <= 256);

// Every `*mut Block` a frame or a shelf holds points at a header written when the block was
// taken, at the start of memory committed for it and owned by that holder alone, until the
// holder gives the block up (`release`, `Shelf::give_span`'s drop). The helpers below rely on
// that.

/// The header of a held block.
fn header<'a>(block: *mut Block) -> &'a Block {
    // SAFETY: by the invariant above `block` is a live header of the caller's; the reference
    // ends within the caller, before the block is given up.
    unsafe { &*block }
}

/// The header of a held block, to change.
#[allow(clippy::mut_from_ref, reason = "a header is reached only through its one holder")]
fn header_mut<'a>(block: *mut Block) -> &'a mut Block {
    // SAFETY: as for `header`; the holder makes no other reference to it meanwhile.
    unsafe { &mut *block }
}

/// The block's first byte of data, with the block's provenance.
fn data(block: *mut Block) -> *mut u8 {
    block.cast::<u8>().wrapping_add(HEADER)
}

/// A bump allocator over a chain of blocks. Not `Sync`: a frame is its task's.
pub struct Frame {
    /// The next free byte, in the current block (or `EMPTY`).
    cursor: Cell<*mut u8>,
    /// The bump's end in the current block: its end, a span's committed end, or the limit's.
    end: Cell<usize>,
    /// The current block's first byte of data; `usize::MAX` with none.
    floor: Cell<usize>,
    /// `used() == cursor + bias`, wrapping.
    bias: Cell<usize>,
    /// The most `used()` was since the last reset.
    peak: Cell<usize>,
    /// Blocks `grow` moved since the last reset, and the bytes it copied moving them.
    copies: Cell<u64>,
    copied: Cell<u64>,
    first: Cell<*mut Block>,
    current: Cell<*mut Block>,
    /// Bytes of the blocks it holds: whole chunks, spans' committed pages.
    held: Cell<usize>,
    /// The most bytes it hands out.
    limit: usize,
    /// The subsystem of the task using it, for [`tag::task_ended`].
    tag: Cell<Tag>,
    id: u32,
    /// A task's frame: its blocks come from and go back to its thread's shelves, under the cap.
    task: bool,
    /// The rest of the current block is lent to an inner task's frame: `end` stops at the
    /// cursor of the lend until that task ends or this frame moves to another block.
    lent: Cell<bool>,
    /// The bytes an outer task's frame lent this one, `start..end`; empty when none.
    window: (usize, usize),
    /// [`sealed`] of `id` while the frame is alive, [`UNSEALED`] once dropped: what [`task`]
    /// checks of the frame a thread says encloses it before using it.
    seal: Cell<u64>,
    /// Where the frame was made: for a task's, the call of [`task`]. Named when a later task
    /// finds this frame dropped where its thread says its enclosing task's is.
    opened_at: &'static Location<'static>,
    /// The thread whose task stack the task's end put back (its [`STACK`]'s address), 0 until it
    /// ends: whether a dropped frame a thread still names was ended on that thread or another.
    ended_on: Cell<usize>,
}

/// The high half of a live frame's seal, its id the low half.
const SEAL: u64 = 0x5ea1_f4a3 << 32;
/// A dropped frame's seal.
const UNSEALED: u64 = 0xdead_f4a3_dead_f4a3;

fn sealed(id: u32) -> u64 {
    SEAL | u64::from(id)
}

// SAFETY: a frame owns its blocks, and every value in them is borrowed from it. A frame handed
// out by value (`Frame::new`) takes and gives its blocks through the process's chunk cache and
// its own regions, which any thread may; a task's frame, the only kind that uses a thread's
// shelves, is never moved: `task` keeps it on its own stack and lends only `&Frame`, which is
// not `Send` since `Frame` is not `Sync`.
unsafe impl Send for Frame {}

/// A point in a frame to roll back to with [`Frame::reset_to`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Marker {
    frame: u32,
    at: usize,
}

impl Frame {
    /// A frame for tasks of `tag`, of at most [`FRAME_RESERVE`]: no memory until it is used.
    #[track_caller]
    pub fn new(tag: Tag) -> Result<Frame, MemError> {
        Frame::with_reserve(FRAME_RESERVE, tag)
    }

    /// A frame of at most `bytes`, which takes blocks as it is used.
    #[track_caller]
    pub fn with_reserve(bytes: usize, tag: Tag) -> Result<Frame, MemError> {
        Ok(Frame::empty(bytes, tag, IDS.fetch_add(1, Ordering::Relaxed), false))
    }

    #[track_caller]
    fn empty(limit: usize, tag: Tag, id: u32, task: bool) -> Frame {
        Frame {
            cursor: Cell::new(ptr::without_provenance_mut(EMPTY)),
            end: Cell::new(EMPTY),
            floor: Cell::new(usize::MAX),
            bias: Cell::new(0usize.wrapping_sub(EMPTY)),
            peak: Cell::new(0),
            copies: Cell::new(0),
            copied: Cell::new(0),
            first: Cell::new(ptr::null_mut()),
            current: Cell::new(ptr::null_mut()),
            held: Cell::new(0),
            limit,
            tag: Cell::new(tag),
            id,
            task,
            lent: Cell::new(false),
            window: (0, 0),
            seal: Cell::new(sealed(id)),
            opened_at: Location::caller(),
            ended_on: Cell::new(0),
        }
    }

    /// A task's frame that starts in the rest of the current chunk of `outer`, the enclosing
    /// task's, when enough is left; else empty. `outer` stops short of the rest until
    /// [`Frame::take_back`].
    #[track_caller]
    fn inner(outer: &Frame, tag: Tag, id: u32) -> (Frame, Option<usize>) {
        let mut frame = Frame::empty(FRAME_RESERVE, tag, id, true);
        let current = outer.current.get();
        let in_span = !current.is_null() && header(current).span.is_some();
        if outer.lent.get() || outer.floor.get() == usize::MAX || in_span {
            return (frame, None);
        }
        let cursor = outer.cursor.get();
        let start = align_up(cursor.addr(), 64);
        let end = outer.end.get();
        if end.saturating_sub(start) < LEND_AT {
            return (frame, None);
        }
        outer.end.set(cursor.addr());
        outer.lent.set(true);
        frame.cursor.set(cursor.with_addr(start));
        frame.end.set(end);
        frame.floor.set(start);
        frame.bias.set(0usize.wrapping_sub(start));
        frame.window = (start, end);
        (frame, Some(end))
    }

    /// The rest of the current block back from the inner task's frame [`Frame::inner`] lent it
    /// to, `end` being where the bump stopped before; nothing when the frame moved on since.
    fn take_back(&self, end: usize) {
        if self.lent.replace(false) {
            self.end.set(end);
        }
    }

    pub fn tag(&self) -> Tag {
        self.tag.get()
    }

    /// Bytes in use since the last reset (alignment padding included).
    #[inline]
    pub fn used(&self) -> usize {
        self.cursor.get().addr().wrapping_add(self.bias.get())
    }

    /// The most bytes in use at once since the last reset.
    pub fn peak(&self) -> usize {
        self.peak.get().max(self.used())
    }

    /// What growing blocks that were not the last copied since the last reset.
    pub fn copied(&self) -> tag::Copied {
        tag::Copied { copies: self.copies.get(), bytes: self.copied.get() }
    }

    /// Folds the bytes in use into `peak`, before the cursor moves back. An allocation never
    /// writes `peak`: `used()` only rises between the points that lower it (freeing or
    /// shrinking the top allocation, a rollback, a reset), each of which folds first, so the
    /// most it reached since the last fold is what it is when the next one runs.
    #[inline]
    fn fold_peak(&self) {
        let used = self.used();
        if used > self.peak.get() {
            self.peak.set(used);
        }
    }

    /// Bytes of the blocks it holds (whole chunks, spans' committed pages), in use or not.
    pub fn committed(&self) -> usize {
        self.held.get()
    }

    /// The most it hands out: [`FRAME_RESERVE`] or what [`Frame::with_reserve`] was given.
    pub fn reserved(&self) -> usize {
        self.limit
    }

    /// Whether `ptr` points into one of this frame's blocks.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let at = ptr.addr();
        if (self.window.0..self.window.1).contains(&at) {
            return true;
        }
        let mut block = self.first.get();
        while !block.is_null() {
            let head = header(block);
            if (data(block).addr()..head.end).contains(&at) {
                return true;
            }
            block = head.next;
        }
        false
    }

    /// Takes `layout`'s bytes, aligned; `MemError` when the frame's limit is reached or memory
    /// cannot be had. A task the cap cancels unwinds with [`Cancelled`] from here.
    #[inline]
    pub fn try_alloc_layout(&self, layout: Layout) -> Result<NonNull<u8>, MemError> {
        let cursor = self.cursor.get();
        let at = align_up(cursor.addr(), layout.align());
        match at.checked_add(layout.size()) {
            Some(end) if end <= self.end.get() => {
                self.cursor.set(cursor.with_addr(end));
                // SAFETY: `at` is in `[cursor, end]`, within the current block's committed bytes,
                // or `EMPTY` (not null) for a zero-sized value in a frame with no block.
                Ok(unsafe { NonNull::new_unchecked(cursor.with_addr(at)) })
            }
            _ => self.alloc_cold(layout),
        }
    }

    #[cold]
    #[inline(never)]
    fn alloc_cold(&self, layout: Layout) -> Result<NonNull<u8>, MemError> {
        self.make_room(layout)?;
        let cursor = self.cursor.get();
        let at = align_up(cursor.addr(), layout.align());
        let end = at + layout.size();
        if end > self.end.get() {
            return Err(self.exhausted(end - cursor.addr()));
        }
        self.cursor.set(cursor.with_addr(end));
        self.peak.set(self.peak.get().max(end.wrapping_add(self.bias.get())));
        // SAFETY: as in `try_alloc_layout`: `at` is within the block `make_room` made current.
        Ok(unsafe { NonNull::new_unchecked(cursor.with_addr(at)) })
    }

    fn exhausted(&self, more: usize) -> MemError {
        MemError::Exhausted { tag: self.tag.get(), reserved: self.limit, wanted: self.used().saturating_add(more) }
    }

    /// Makes the current block one `layout` fits in at the cursor: the current span grown, a
    /// block ahead (kept from before a rollback or reset), or a new one.
    fn make_room(&self, layout: Layout) -> Result<(), MemError> {
        let (size, align) = (layout.size(), layout.align());
        if align > MAX_ALIGN {
            return Err(MemError::TooLarge { tag: self.tag.get(), what: "a frame allocation's alignment", wanted: align });
        }
        if self.used().saturating_add(size) > self.limit {
            return Err(self.exhausted(size));
        }
        let current = self.current.get();
        if !current.is_null() {
            let at = align_up(self.cursor.get().addr(), align);
            if header(current).span.is_some() && at.saturating_add(size) <= header(current).end {
                return self.commit_span(current, at + size);
            }
            let mut ahead = header(current).next;
            while !ahead.is_null() {
                if fits(ahead, layout) {
                    self.enter(ahead);
                    return self.reach(ahead, layout);
                }
                ahead = header(ahead).next;
            }
        }
        let block = if size > SPAN_AT || !chunk_fits(layout) { self.new_span(layout)? } else { self.new_chunk()? };
        self.link_after_current(block);
        self.enter(block);
        self.reach(block, layout)
    }

    /// Commits what `layout` needs at the cursor of the span `block`, just made current.
    fn reach(&self, block: *mut Block, layout: Layout) -> Result<(), MemError> {
        if header(block).span.is_some() {
            let at = align_up(self.cursor.get().addr(), layout.align());
            return self.commit_span(block, at + layout.size());
        }
        Ok(())
    }

    /// Commits the current span `block` up to address `end` (reserving the pages from the cap
    /// first, for a task's frame) and moves the bump's end there.
    fn commit_span(&self, block: *mut Block, end: usize) -> Result<(), MemError> {
        let Some(region) = header(block).span.as_deref() else {
            unreachable!("a chunk has no pages to commit")
        };
        let len = end - region.base().addr().get();
        let before = region.committed();
        let more = region.committed_after(len)? - before;
        let granted = match self.task && more > 0 {
            true => cap::frame_reserve(more).map_err(|why| refused(why, self.tag.get(), more))?,
            false => false,
        };
        if let Err(error) = region.ensure(len) {
            if granted {
                cap::frame_release(more);
            }
            return Err(error);
        }
        let grew = region.committed() - before;
        if granted {
            header_mut(block).granted += more;
        }
        self.held.set(self.held.get() + grew);
        self.end.set(self.usable_end(block, header(block).before));
        Ok(())
    }

    /// Where the bump of `block` must stop: its end (a span's committed end), or the limit's.
    fn usable_end(&self, block: *mut Block, before: usize) -> usize {
        let head = header(block);
        let end = match head.span.as_deref() {
            Some(region) => region.base().addr().get() + region.committed(),
            None => head.end,
        };
        end.min(data(block).addr().saturating_add(self.limit.saturating_sub(before)))
    }

    /// Makes `block` current at its start, after what the frame used so far.
    fn enter(&self, block: *mut Block) {
        self.place(block, self.used(), data(block));
    }

    /// Makes `block` current with its cursor at `at`, `before` bytes used before it.
    fn place(&self, block: *mut Block, before: usize, at: *mut u8) {
        header_mut(block).before = before;
        let start = data(block).addr();
        self.current.set(block);
        self.cursor.set(at);
        self.floor.set(start);
        self.bias.set(before.wrapping_sub(start));
        self.end.set(self.usable_end(block, before));
        self.lent.set(false);
    }

    fn link_after_current(&self, block: *mut Block) {
        let current = self.current.get();
        let head = header_mut(block);
        if current.is_null() {
            head.prev = ptr::null_mut();
            head.next = self.first.get();
            if !head.next.is_null() {
                header_mut(head.next).prev = block;
            }
            self.first.set(block);
            return;
        }
        let next = header(current).next;
        head.prev = current;
        head.next = next;
        if !next.is_null() {
            header_mut(next).prev = block;
        }
        header_mut(current).next = block;
    }

    fn new_chunk(&self) -> Result<*mut Block, MemError> {
        let chunk = match self.task {
            true => match SHELF_OF.try_with(|shelf| shelf.take_chunk(self.tag.get())) {
                Ok(taken) => taken?,
                Err(_) => chunks::take(Tag::Frames)?,
            },
            false => chunks::take(Tag::Frames)?,
        };
        let base = chunk.ptr();
        let block = base.cast::<Block>().as_ptr();
        // SAFETY: the chunk is ours alone, committed, `CHUNK` bytes and aligned to `CHUNK`, so
        // its start holds a header; what it held before is dead.
        unsafe {
            block.write(Block {
                prev: ptr::null_mut(),
                next: ptr::null_mut(),
                before: 0,
                end: base.addr().get() + CHUNK,
                chunk: chunk.index(),
                granted: 0,
                span: None,
            })
        };
        self.held.set(self.held.get() + CHUNK);
        Ok(block)
    }

    /// A span for `layout`: a warm one of the thread's that is big enough, or a new region
    /// reserved far past it, on the thread's node; its pages for `layout` committed.
    fn new_span(&self, layout: Layout) -> Result<*mut Block, MemError> {
        let tag = self.tag.get();
        let need = HEADER
            .checked_add(layout.align())
            .and_then(|it| it.checked_add(layout.size()))
            .ok_or(MemError::TooLarge { tag, what: "a frame allocation", wanted: layout.size() })?;
        if self.task
            && let Ok(Some(block)) = SHELF_OF.try_with(|shelf| shelf.take_span(need))
        {
            let head = header_mut(block);
            let committed = head.span.as_deref().map_or(0, Region::committed);
            match cap::frame_reserve(committed) {
                Ok(granted) => head.granted = if granted { committed } else { 0 },
                Err(why) => {
                    _ = SHELF_OF.try_with(|shelf| shelf.give_span(block));
                    return Err(refused(why, tag, committed));
                }
            }
            head.prev = ptr::null_mut();
            head.next = ptr::null_mut();
            self.held.set(self.held.get() + committed);
            return Ok(block);
        }
        let reserve = span_reserve(need);
        let region = match chunks::thread_node() {
            Some(node) => Region::reserve_huge_on(reserve, node, Tag::Frames)?,
            None => Region::reserve_huge(reserve, Tag::Frames)?,
        };
        let first = region.committed_after(need)?;
        let granted = match self.task {
            true => cap::frame_reserve(first).map_err(|why| refused(why, tag, first))?,
            false => false,
        };
        if let Err(error) = region.ensure(need) {
            if granted {
                cap::frame_release(first);
            }
            return Err(error);
        }
        let base = region.base();
        let end = base.addr().get() + region.reserved();
        let committed = region.committed();
        let block = base.cast::<Block>().as_ptr();
        // SAFETY: the region is fresh, committed for `need >= HEADER` bytes and aligned to a
        // page, so its start holds a header; nothing else knows of it.
        unsafe {
            block.write(Block {
                prev: ptr::null_mut(),
                next: ptr::null_mut(),
                before: 0,
                end,
                chunk: crate::stack::NONE,
                granted: if granted { committed } else { 0 },
                span: Some(ManuallyDrop::new(region)),
            })
        };
        self.held.set(self.held.get() + committed);
        Ok(block)
    }

    /// Gives up `block`, which is out of the chain: a chunk back to the thread's shelf or the
    /// cache, a span back to the thread's shelf of spans or to the kernel. With `cold`, its pages
    /// go to the kernel.
    fn release(&self, block: *mut Block, cold: bool) {
        let head = header_mut(block);
        if head.span.is_none() {
            self.held.set(self.held.get() - CHUNK);
            let index = head.chunk;
            let given = self.task
                && SHELF_OF.try_with(|shelf| shelf.give_chunk(Chunk::from_index(index), cold)).is_ok();
            if !given {
                match cold {
                    true => chunks::give_cold(Chunk::from_index(index), Tag::Frames),
                    false => chunks::give(Chunk::from_index(index), Tag::Frames),
                }
            }
            return;
        }
        let committed = head.span.as_deref().map_or(0, Region::committed);
        self.held.set(self.held.get() - committed);
        if head.granted > 0 {
            cap::frame_release(head.granted);
            head.granted = 0;
        }
        if self.task && !cold && SHELF_OF.try_with(|shelf| shelf.give_span(block)).is_ok() {
            return;
        }
        drop_span(block);
    }

    /// Stops with the error when memory cannot be had.
    #[inline]
    #[track_caller]
    pub fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
        match self.try_alloc_layout(layout) {
            Ok(ptr) => ptr,
            Err(error) => fail(&error),
        }
    }

    /// Places `value` in the frame. Its destructor never runs: the frame forgets it at reset.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc<T>(&self, value: T) -> &mut T {
        let ptr = self.alloc_layout(Layout::new::<T>()).cast::<T>();
        // SAFETY: `ptr` is fresh memory of this frame, sized and aligned for `T`, handed out
        // once; it stays valid until a reset, which takes `&mut self` and so outlives this
        // borrow.
        unsafe { ptr.write(value) };
        // SAFETY: just initialized, and no other reference to it exists.
        unsafe { &mut *ptr.as_ptr() }
    }

    /// Copies `values` into the frame.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_slice_copy<T: Copy>(&self, values: &[T]) -> &mut [T] {
        let slots = self.alloc_uninit_slice::<T>(values.len());
        for (slot, value) in slots.iter_mut().zip(values) {
            slot.write(*value);
        }
        // SAFETY: every element was just written.
        unsafe { slots.assume_init_mut() }
    }

    /// `len` values made by `f(0)`, `f(1)`, ... in the frame. If `f` panics, the values made
    /// so far are forgotten with the frame, as every frame value is.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_slice_fill_with<T>(&self, len: usize, mut f: impl FnMut(usize) -> T) -> &mut [T] {
        let slots = self.alloc_uninit_slice::<T>(len);
        for (i, slot) in slots.iter_mut().enumerate() {
            slot.write(f(i));
        }
        // SAFETY: every element was just written.
        unsafe { slots.assume_init_mut() }
    }

    /// The items of `iter`, which says its length, in the frame.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_slice_from_iter<T, I>(&self, iter: I) -> &mut [T]
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        let mut iter = iter.into_iter();
        let len = iter.len();
        let slots = self.alloc_uninit_slice::<T>(len);
        let mut filled = 0;
        for slot in slots.iter_mut() {
            match iter.next() {
                Some(value) => {
                    slot.write(value);
                    filled += 1;
                }
                None => break,
            }
        }
        // SAFETY: the first `filled` elements were written; an iterator that said more than it
        // gave leaves the rest out of the slice.
        unsafe { slots[..filled].assume_init_mut() }
    }

    /// Room for `len` values of `T`, uninitialized.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_uninit_slice<T>(&self, len: usize) -> &mut [MaybeUninit<T>] {
        let layout = match Layout::array::<T>(len) {
            Ok(layout) => layout,
            Err(_) => fail(&MemError::TooLarge { tag: self.tag(), what: "a frame slice", wanted: len }),
        };
        let ptr = self.alloc_layout(layout).cast::<MaybeUninit<T>>();
        // SAFETY: fresh memory of this frame for `len` elements, aligned, handed out once and
        // valid until a reset (which takes `&mut self`); `MaybeUninit` needs no initialization.
        unsafe { slice::from_raw_parts_mut(ptr.as_ptr(), len) }
    }

    /// Copies `text` into the frame.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_str(&self, text: &str) -> &mut str {
        let bytes = self.alloc_slice_copy(text.as_bytes());
        // SAFETY: the bytes are a copy of a `str`'s, so UTF-8.
        unsafe { str::from_utf8_unchecked_mut(bytes) }
    }

    /// The point to roll back to.
    pub fn mark(&self) -> Marker {
        Marker { frame: self.id, at: self.cursor.get().addr() }
    }

    /// Frees everything allocated since `marker` was taken; the blocks taken since stay the
    /// frame's, for what comes next. Stops when the marker is another frame's or from after a
    /// rollback past it.
    #[track_caller]
    pub fn reset_to(&mut self, marker: Marker) {
        assert_eq!(marker.frame, self.id, "a marker of another frame");
        assert!(
            self.roll_back(marker.at),
            "a marker taken after a point the frame was already rolled back past"
        );
    }

    /// Moves the cursor back to address `at`, in the current block or one before it. Whether
    /// `at` was such a point.
    fn roll_back(&self, at: usize) -> bool {
        self.fold_peak();
        let cursor = self.cursor.get();
        if (self.floor.get()..=cursor.addr()).contains(&at) {
            self.cursor.set(cursor.with_addr(at));
            return true;
        }
        if at == EMPTY {
            self.rewind();
            return true;
        }
        let current = self.current.get();
        if current.is_null() {
            return false;
        }
        let mut block = header(current).prev;
        while !block.is_null() {
            let head = header(block);
            if (data(block).addr()..=head.end).contains(&at) {
                self.place(block, head.before, data(block).with_addr(at));
                return true;
            }
            block = head.prev;
        }
        false
    }

    /// The cursor back at the first block's start (or the empty frame's), the blocks kept.
    fn rewind(&self) {
        let first = self.first.get();
        if first.is_null() {
            self.cursor.set(ptr::without_provenance_mut(EMPTY));
            self.end.set(EMPTY);
            self.floor.set(usize::MAX);
            self.bias.set(0usize.wrapping_sub(EMPTY));
            return;
        }
        self.place(first, 0, data(first));
    }

    /// Frees everything, keeping its blocks for the next task. Counts the task's peak to its
    /// tag ([`tag::task_ended`]).
    pub fn reset(&mut self) {
        tag::task_ended(self.tag.get(), self.peak(), self.copied());
        self.rewind();
        self.peak.set(0);
        self.copies.set(0);
        self.copied.set(0);
    }

    /// Runs `f` with this frame and frees what it allocated once it returns. What `f` returns
    /// cannot borrow the frame, so nothing it allocated outlives the rollback.
    pub fn scope<R>(&mut self, f: impl FnOnce(&Frame) -> R) -> R {
        struct Back<'f> {
            frame: &'f Frame,
            at: usize,
        }
        impl Drop for Back<'_> {
            fn drop(&mut self) {
                let back = self.frame.roll_back(self.at);
                debug_assert!(back, "a scope's own mark is a point of its frame");
            }
        }
        let back = Back { frame: self, at: self.cursor.get().addr() };
        f(back.frame)
    }

    /// Gives back to the kernel the blocks ahead of the cursor and the current span's pages
    /// past it, until it holds at most `keep` bytes (or what is in use, if more).
    pub fn trim(&mut self, keep: usize) {
        let current = self.current.get();
        let mut last = if current.is_null() { self.first.get() } else { header(current).next };
        while !last.is_null() && !header(last).next.is_null() {
            last = header(last).next;
        }
        while !last.is_null() && last != current && self.held.get() > keep {
            let prev = header(last).prev;
            match prev.is_null() {
                true => self.first.set(ptr::null_mut()),
                false => header_mut(prev).next = ptr::null_mut(),
            }
            self.release(last, true);
            last = prev;
        }
        if current.is_null() || self.held.get() <= keep {
            return;
        }
        let head = header_mut(current);
        if let Some(region) = head.span.as_deref_mut() {
            let before = region.committed();
            let in_use = self.cursor.get().addr() - region.base().addr().get();
            // A refused decommit leaves the pages committed: memory held, nothing broken.
            _ = region.decommit_after(in_use.max(HEADER));
            let freed = before - region.committed();
            self.held.set(self.held.get() - freed);
            let back = freed.min(head.granted);
            if back > 0 {
                cap::frame_release(back);
                head.granted -= back;
            }
            self.end.set(self.usable_end(current, head.before));
        }
    }

    pub(crate) fn set_tag(&self, tag: Tag) {
        self.tag.set(tag);
    }

    /// Whether `[ptr, ptr + size)` ends at the cursor, in the current block: the most recent
    /// allocation.
    #[inline]
    fn is_top(&self, ptr: NonNull<u8>, size: usize) -> bool {
        ptr.addr().get() + size == self.cursor.get().addr() && ptr.addr().get() >= self.floor.get()
    }

    /// Grows the top allocation, ending at the cursor, to end at `end`, in place: within the
    /// bump's end, or in the current span by committing more. Whether it could.
    fn grow_top(&self, end: usize) -> Result<bool, MemError> {
        if end > self.end.get() {
            let current = self.current.get();
            if current.is_null() || header(current).span.is_none() || end > header(current).end {
                return Ok(false);
            }
            if self.used().saturating_add(end - self.cursor.get().addr()) > self.limit {
                return Ok(false);
            }
            self.commit_span(current, end)?;
            if end > self.end.get() {
                return Ok(false);
            }
        }
        self.cursor.set(self.cursor.get().with_addr(end));
        self.peak.set(self.peak.get().max(end.wrapping_add(self.bias.get())));
        Ok(true)
    }
}

/// Whether a fresh chunk holds `layout` after its header.
fn chunk_fits(layout: Layout) -> bool {
    layout.align() <= CHUNK && align_up(HEADER, layout.align()).saturating_add(layout.size()) <= CHUNK
}

/// Whether the block `block` holds `layout` at its start.
fn fits(block: *mut Block, layout: Layout) -> bool {
    let at = align_up(data(block).addr(), layout.align());
    at.checked_add(layout.size()).is_some_and(|end| end <= header(block).end)
}

/// The address space a span for `need` bytes reserves: room to grow in place to four times
/// the next power of two, at least 64 MB.
#[cfg(target_pointer_width = "64")]
fn span_reserve(need: usize) -> usize {
    need.checked_next_power_of_two().map_or(need, |it| it.saturating_mul(4)).max(64 << 20)
}

/// On a 32-bit target a reservation is memory taken whole: a span is what it holds.
#[cfg(not(target_pointer_width = "64"))]
fn span_reserve(need: usize) -> usize {
    need
}

/// Gives a span's region back to the kernel, its header with it.
fn drop_span(block: *mut Block) {
    let head = header_mut(block);
    let Some(region) = head.span.take() else { unreachable!("a span holds its region") };
    // The header is inside the region: nothing reads it once the region goes.
    drop(ManuallyDrop::into_inner(region));
}

/// Why the cap refused, as the error to return; a cancel unwinds instead.
fn refused(why: cap::Refused, tag: Tag, wanted: usize) -> MemError {
    match why {
        cap::Refused::Cancelled => cancel(),
        cap::Refused::TooBig { limit } => {
            MemError::Exhausted { tag, reserved: usize::try_from(limit).unwrap_or(usize::MAX), wanted }
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let mut block = self.first.get();
        self.first.set(ptr::null_mut());
        self.current.set(ptr::null_mut());
        while !block.is_null() {
            let next = header(block).next;
            self.release(block, false);
            block = next;
        }
        // SAFETY: `seal` is this frame's own field, alive until `drop` returns. Volatile, as a
        // store to an object about to die is otherwise one the compiler may drop, and a stale
        // pointer to this frame must find it unsealed.
        unsafe { self.seal.as_ptr().write_volatile(UNSEALED) };
    }
}

// SAFETY: blocks are disjoint ranges of the frame's chunks and spans, handed out once each and
// committed before they are returned. A block stays valid until it is deallocated or the frame
// is reset, rolled back or dropped, each of which takes `&mut Frame` (or ownership), which no
// live `&Frame` an allocation borrows allows; growing commits a span further, never moving it.
// `deallocate` and `shrink` only move the cursor back over the top block, never touching
// another. A cancel by the cap unwinds before any of the frame's state changes.
unsafe impl Allocator for Frame {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let ptr = self.try_alloc_layout(layout).map_err(|_| AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if self.is_top(ptr, layout.size()) {
            self.fold_peak();
            self.cursor.set(self.cursor.get().with_addr(ptr.addr().get()));
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
            && let Some(end) = ptr.addr().get().checked_add(new.size())
            && self.grow_top(end).map_err(|_| AllocError)?
        {
            return Ok(NonNull::slice_from_raw_parts(ptr, new.size()));
        }
        let to = self.try_alloc_layout(new).map_err(|_| AllocError)?;
        // SAFETY: the caller says `ptr` is a live block of `old.size()` bytes; `to` is a fresh
        // block of at least as many, so the two do not overlap.
        unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), to.as_ptr(), old.size()) };
        self.copies.set(self.copies.get() + 1);
        self.copied.set(self.copied.get() + old.size() as u64);
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
        // SAFETY: the block holds `new.size()` bytes, so `[old.size(), new.size())` is inside
        // it; memory reused after a reset may hold old bytes, hence the explicit zeroing.
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
            // SAFETY: `ptr` is a live block of at least `new.size()` bytes and `to` a fresh one.
            unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), to.as_ptr(), new.size()) };
            return Ok(NonNull::slice_from_raw_parts(to, new.size()));
        }
        if self.is_top(ptr, old.size()) {
            self.fold_peak();
            self.cursor.set(self.cursor.get().with_addr(ptr.addr().get() + new.size()));
        }
        Ok(NonNull::slice_from_raw_parts(ptr, new.size()))
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("tag", &self.tag.get())
            .field("used", &self.used())
            .field("peak", &self.peak())
            .field("committed", &self.committed())
            .field("reserved", &self.limit)
            .finish()
    }
}

/// A thread's memory for its tasks' frames between tasks: chunks, and spans with their pages
/// committed. Given back to the process when the thread ends.
struct Shelf {
    chunks: [Cell<u32>; SHELF],
    len: Cell<usize>,
    /// Spans, linked through their headers' `next`.
    spans: Cell<*mut Block>,
    span_count: Cell<usize>,
    /// Chunks the thread holds, on this shelf and in its tasks' frames.
    held: Cell<usize>,
    /// How many of those are reserved from the cap: those past the first [`SHELF`].
    capped: Cell<usize>,
}

impl Shelf {
    /// A chunk for a task's frame: the shelf's top, else one from the cache, reserved from the
    /// cap when the thread holds [`SHELF`] already.
    fn take_chunk(&self, tag: Tag) -> Result<Chunk, MemError> {
        let len = self.len.get();
        if len > 0 {
            self.len.set(len - 1);
            return Ok(Chunk::from_index(self.chunks[len - 1].get()));
        }
        let capped = match self.held.get() >= SHELF {
            true => cap::frame_reserve(CHUNK).map_err(|why| refused(why, tag, CHUNK))?,
            false => false,
        };
        let chunk = match chunks::take(Tag::Frames) {
            Ok(chunk) => chunk,
            Err(error) => {
                if capped {
                    cap::frame_release(CHUNK);
                }
                return Err(error);
            }
        };
        self.held.set(self.held.get() + 1);
        if capped {
            self.capped.set(self.capped.get() + 1);
        }
        Ok(chunk)
    }

    /// Keeps a chunk a task's frame gave up, or gives it to the cache when the shelf is full
    /// (to the kernel's pages with `cold`).
    fn give_chunk(&self, chunk: Chunk, cold: bool) {
        let len = self.len.get();
        if len < SHELF && !cold {
            self.chunks[len].set(chunk.index());
            self.len.set(len + 1);
            return;
        }
        match cold {
            true => chunks::give_cold(chunk, Tag::Frames),
            false => chunks::give(chunk, Tag::Frames),
        }
        let held = self.held.get().saturating_sub(1);
        self.held.set(held);
        if self.capped.get() > held.saturating_sub(SHELF) {
            cap::frame_release(CHUNK);
            self.capped.set(self.capped.get() - 1);
        }
    }

    /// A warm span whose reservation holds `need` bytes, out of the shelf.
    fn take_span(&self, need: usize) -> Option<*mut Block> {
        let mut prev: *mut Block = ptr::null_mut();
        let mut span = self.spans.get();
        while !span.is_null() {
            let head = header(span);
            if head.end - data(span).addr() >= need {
                match prev.is_null() {
                    true => self.spans.set(head.next),
                    false => header_mut(prev).next = head.next,
                }
                self.span_count.set(self.span_count.get() - 1);
                return Some(span);
            }
            prev = span;
            span = head.next;
        }
        None
    }

    /// Keeps a span a task's frame gave up (its cap bytes given back), or gives it to the
    /// kernel when the shelf is full.
    fn give_span(&self, span: *mut Block) {
        if self.span_count.get() >= SPANS {
            drop_span(span);
            return;
        }
        let head = header_mut(span);
        head.prev = ptr::null_mut();
        head.next = self.spans.get();
        self.spans.set(span);
        self.span_count.set(self.span_count.get() + 1);
    }

    /// Keeps at most `keep` bytes: the chunks past it go to the cache with their pages given
    /// to the kernel, the spans past it go to the kernel, and the last span kept is decommitted
    /// down to what is left of `keep`.
    fn trim(&self, keep: usize) {
        let keep_chunks = (keep / CHUNK).min(SHELF);
        while self.len.get() > keep_chunks {
            let len = self.len.get() - 1;
            self.len.set(len);
            let chunk = Chunk::from_index(self.chunks[len].get());
            self.give_chunk(chunk, true);
        }
        let mut left = keep - keep_chunks * CHUNK;
        let mut prev: *mut Block = ptr::null_mut();
        let mut span = self.spans.get();
        while !span.is_null() {
            let head = header_mut(span);
            let next = head.next;
            let committed = head.span.as_deref().map_or(0, Region::committed);
            if left >= committed {
                left -= committed;
                prev = span;
            } else if left >= HEADER {
                if let Some(region) = head.span.as_deref_mut() {
                    // A refused decommit leaves the pages committed: memory held, nothing broken.
                    _ = region.decommit_after(left);
                }
                left = 0;
                prev = span;
            } else {
                match prev.is_null() {
                    true => self.spans.set(next),
                    false => header_mut(prev).next = next,
                }
                self.span_count.set(self.span_count.get() - 1);
                drop_span(span);
            }
            span = next;
        }
    }

    fn count(&self) -> usize {
        self.len.get() + self.span_count.get()
    }
}

impl Drop for Shelf {
    fn drop(&mut self) {
        for at in 0..self.len.get() {
            chunks::give(Chunk::from_index(self.chunks[at].get()), Tag::Frames);
        }
        self.len.set(0);
        let mut span = self.spans.get();
        self.spans.set(ptr::null_mut());
        while !span.is_null() {
            let next = header(span).next;
            drop_span(span);
            span = next;
        }
    }
}

/// The thread's tasks: the innermost one's frame, what the frames enclosing it used when it
/// began, and the most the thread's frames held at once since the innermost [`measure`] began.
struct Stack {
    current: Cell<*const Frame>,
    below: Cell<usize>,
    peak: Cell<usize>,
    /// Frame ids handed out to this thread's tasks, `next..end`.
    ids: Cell<(u32, u32)>,
    /// A cap's cancel is unwinding this thread: set by [`cancel`], cleared when the thread's
    /// outermost task ends.
    cancelling: Cell<bool>,
}

thread_local! {
    static SHELF_OF: Shelf = const {
        Shelf {
            chunks: [const { Cell::new(0) }; SHELF],
            len: Cell::new(0),
            spans: Cell::new(ptr::null_mut()),
            span_count: Cell::new(0),
            held: Cell::new(0),
            capped: Cell::new(0),
        }
    };
    static STACK: Stack = const {
        Stack {
            current: Cell::new(ptr::null()),
            below: Cell::new(0),
            peak: Cell::new(0),
            ids: Cell::new((0, 0)),
            cancelling: Cell::new(false),
        }
    };
}

/// An id for a task's frame: from a block of 1024 the thread takes at once, so a task does not
/// write a shared line for one.
fn task_id(stack: &Stack) -> u32 {
    let (next, end) = stack.ids.get();
    if next != end {
        stack.ids.set((next.wrapping_add(1), end));
        return next;
    }
    let start = IDS.fetch_add(1024, Ordering::Relaxed);
    stack.ids.set((start.wrapping_add(1), start.wrapping_add(1024)));
    start
}

/// The innermost task's frame of the calling thread, if it is in one.
///
/// # Safety
///
/// The reference is used only while the task that made it current is running, on this
/// thread: within a call made from inside its closure.
#[inline(never)]
unsafe fn innermost<'a>() -> Option<&'a Frame> {
    let frame = STACK.try_with(|stack| stack.current.get()).unwrap_or(ptr::null());
    // SAFETY: `task` points `current` at the frame its `Back` holds on `task`'s own stack (never
    // moved while its closure runs) and points it back at the outer task's before that frame
    // is dropped. The caller uses it only within that closure, deeper on the same thread's
    // stack, so the frame is alive meanwhile, and no reset happens: a reset takes `&mut Frame`,
    // which nothing has while `task` lends `&Frame`.
    unsafe { frame.as_ref() }
}

/// Panics, naming the frame, its owner and task `starting`, when `outer`, the frame the thread
/// says encloses `starting`, is not a live task's frame: one dropped, or memory that never was a
/// frame. That is a thread-local carried wrong across a job's switch (a job resumed or started
/// on a thread whose current frame is another job's), which used as is corrupts both.
#[cold]
#[inline(never)]
fn refuse_unmapped(outer: *const Frame, starting: u32) -> ! {
    panic!(
        "blaze-mem: task {starting} starts inside the frame at {outer:p}, which this thread's \
         stack names as the enclosing task's, and no frame can be there: it is {}; a task frame \
         was carried wrong across a job's switch, or a number was stored as a frame. Task \
         {starting} starts at:\n{}",
        if outer.addr() < LOWEST { "below the lowest address mapped" } else { "not aligned as a frame is" },
        std::backtrace::Backtrace::force_capture()
    )
}

/// No address below this is mapped: Linux's default `vm.mmap_min_addr`, and far below macOS's
/// zero page.
const LOWEST: usize = 1 << 16;

#[cold]
#[inline(never)]
fn refuse_outer(outer: *const Frame, seal: u64, owner: u32, starting: u32) -> ! {
    let why = if seal == UNSEALED { "dropped" } else { "not a frame, or overwritten" };
    // A frame's drop writes only its seal, so a frame sealed as dropped still has its other
    // bytes as they were; any other seal says the bytes are not a frame's, and nothing more is
    // read of them.
    let owned = if seal == UNSEALED {
        let opened_at = outer.wrapping_byte_add(mem::offset_of!(Frame, opened_at)).cast::<&'static Location<'static>>();
        let ended_on = outer.wrapping_byte_add(mem::offset_of!(Frame, ended_on)).cast::<usize>();
        // SAFETY: the `opened_at` field of the dropped frame's bytes, as they were when it was
        // alive: a `&'static Location`, which a drop leaves as it was.
        let opened_at = unsafe { opened_at.read_volatile() };
        // SAFETY: the `ended_on` field of the same bytes; a `Cell<usize>` is a `usize`.
        let ended_on = unsafe { ended_on.read_volatile() };
        let here = STACK.try_with(|stack| ptr::from_ref(stack).addr()).unwrap_or(0);
        let ended = match ended_on {
            0 => "ended by no task's end",
            on if on == here => "ended on this thread",
            _ => "ended on another thread",
        };
        // alloc: once, for the panic's message
        format!(" Task {owner} was opened at {opened_at} and {ended}.")
    } else {
        String::new()
    };
    panic!(
        "blaze-mem: task {starting} starts inside the frame at {outer:p}, which this thread's \
         stack names as the enclosing task's, and it is {why}: its seal is {seal:#x}, where a \
         live frame of its owner, task {owner}, has {:#x}; a task frame was carried wrong \
         across a job's switch.{owned} The frame's address is {}. Task {starting} starts at:\n{}",
        sealed(owner),
        whereabouts(outer),
        std::backtrace::Backtrace::force_capture()
    )
}

/// Where `at` lies against the calling thread's stack pointer: the frame of a call still under
/// way on this stack is above it, by less than the stack's size; one below it is of a call
/// that returned, and one far either way is on another stack (another fiber's, a segment
/// `stdx::ensure_sufficient_stack` grew) or not on a stack.
#[inline(never)]
fn whereabouts(at: *const Frame) -> String {
    let here = 0u8;
    let sp = ptr::from_ref(&here).addr();
    let (by, side) = match at.addr().checked_sub(sp) {
        Some(above) => (above, "above"),
        None => (sp - at.addr(), "below"),
    };
    // alloc: once, for a panic's message
    format!("{by:#x} bytes {side} this thread's stack pointer {sp:#x}")
}

/// The seal and the owner's id in the frame-sized, aligned bytes at `at`, as the memory holds
/// them now: only the two fields are read, at their offsets, with no reference to a frame made.
fn read_seal(at: *const Frame) -> (u64, u32) {
    // `at` points at a live frame, or, when a thread-local was carried wrong, at the stack
    // memory a dropped one was in: a fiber's stack, which the scheduler keeps mapped for its
    // next job, so the reads are of mapped bytes (were they not, the read faults and the
    // daemon's signal line names it). Volatile, so each read is of the memory now and not a
    // value the compiler assumed of a frame it believes alive.
    let seal_at = at.wrapping_byte_add(mem::offset_of!(Frame, seal)).cast::<u64>();
    let id_at = at.wrapping_byte_add(mem::offset_of!(Frame, id)).cast::<u32>();
    // SAFETY: `seal_at` is the `seal` field of the frame-sized, aligned bytes at `at`, as
    // above; a `Cell<u64>` is a `u64`.
    let seal = unsafe { seal_at.read_volatile() };
    // SAFETY: `id_at` is the `id` field of the same bytes, as above.
    let owner = unsafe { id_at.read_volatile() };
    (seal, owner)
}

/// Panics, naming the switch, when the frame a job carries across it ([`take_current`],
/// [`put_current`]) is not a live task's frame, so a frame carried wrong fails at the switch
/// that carries it rather than at the next task that starts inside it.
#[inline(never)]
fn check_carried(frame: *const Frame, switch: &'static str) {
    if frame.is_null() {
        return;
    }
    let why = if frame.addr() < LOWEST {
        "below the lowest address mapped"
    } else if !frame.is_aligned() {
        "not aligned as a frame is"
    } else {
        let (seal, owner) = read_seal(frame);
        if seal == sealed(owner) {
            return;
        }
        if seal == UNSEALED { "dropped" } else { "not a frame, or overwritten" }
    };
    panic!(
        "blaze-mem: a job {switch} carrying the frame at {frame:p} as its innermost task's, and it \
         is {why}; the frame's address is {}. The switch is at:\n{}",
        whereabouts(frame),
        std::backtrace::Backtrace::force_capture()
    )
}

/// [`refuse_outer`] unless `outer` is null or a live frame.
#[inline]
fn check_outer(outer: *const Frame, starting: u32) {
    if outer.is_null() {
        return;
    }
    if outer.addr() < LOWEST || !outer.is_aligned() {
        refuse_unmapped(outer, starting);
    }
    let (seal, owner) = read_seal(outer);
    if seal != sealed(owner) {
        refuse_outer(outer, seal, owner, starting);
    }
}

/// Runs a task of `tag` with a frame of its own, empty, and frees it when `f` returns or
/// unwinds. A task inside a task, at any depth, gets another frame, so the inner end frees only
/// the inner task's memory. What `f` returns cannot borrow the frame.
///
/// Once the thread's shelves are warm a task makes no system call and no global allocation:
/// its frame is a header here, and its blocks come from and go back to the shelves (or the
/// process's chunk cache, one compare-and-swap each).
#[track_caller]
pub fn task<R>(tag: Tag, f: impl FnOnce(&Frame) -> R) -> R {
    struct Back {
        frame: Frame,
        outer: *const Frame,
        /// What the enclosing frames used when this task began, and the outer one's part.
        below: usize,
        outer_below: usize,
        outer_used: usize,
        /// Where the outer frame's bump stopped before it lent this one the rest of its chunk.
        lent: Option<usize>,
    }
    impl Drop for Back {
        fn drop(&mut self) {
            // SAFETY: `outer` is the enclosing task's frame, alive until that task ends, which
            // is after this one; null outside any.
            let outer = unsafe { self.outer.as_ref() };
            let outer_now = outer.map_or(0, Frame::used);
            // Nothing of this task's is used past here: what `f` returned cannot borrow its
            // frame, so the lent bytes are the outer frame's again.
            if let (Some(outer), Some(end)) = (outer, self.lent) {
                outer.take_back(end);
            }
            // What the outer frame grew by while this task ran, counted on top of this one's
            // peak: the two may not have been at once, so this errs high.
            let grew = outer_now.saturating_sub(self.outer_used);
            let held = self.below + self.frame.peak() + grew;
            end_task(&self.frame, self.outer, self.outer_below, held);
            tag::task_ended(self.frame.tag(), self.frame.peak(), self.frame.copied());
            // Dropping the frame, after this, gives its blocks to the thread's shelves.
        }
    }
    let (outer, outer_below, id) = begin_task();
    check_outer(outer, id);
    // SAFETY: `outer` is the enclosing task's frame, alive while this call, made from inside
    // its closure, runs; null outside any. `check_outer` refused one that is not.
    let outer_frame = unsafe { outer.as_ref() };
    let outer_used = outer_frame.map_or(0, Frame::used);
    let below = outer_below + outer_used;
    let (frame, lent) = match outer_frame {
        Some(outer) => Frame::inner(outer, tag, id),
        None => (Frame::empty(FRAME_RESERVE, tag, id, true), None),
    };
    let back = Back { frame, outer, below, outer_below, outer_used, lent };
    enter_task(&back.frame, below);
    f(&back.frame)
}

// A job can resume on another thread after any wait inside a task, so every access to `STACK`
// is a call of its own that ends before any wait: inlined into a task's caller, the
// thread-local's address could be kept from before the wait.

/// The thread's innermost frame, what the frames enclosing it use, and a new task's id.
#[inline(never)]
fn begin_task() -> (*const Frame, usize, u32) {
    STACK
        .try_with(|stack| (stack.current.get(), stack.below.get(), task_id(stack)))
        .unwrap_or_else(|_| (ptr::null(), 0, IDS.fetch_add(1, Ordering::Relaxed)))
}

#[inline(never)]
fn enter_task(frame: &Frame, below: usize) {
    _ = STACK.try_with(|stack| {
        stack.current.set(frame);
        stack.below.set(below);
    });
}

/// `frame`'s task ended on this thread: `outer` is innermost again, and the thread's peak is
/// at least `held`.
#[inline(never)]
fn end_task(frame: &Frame, outer: *const Frame, outer_below: usize, held: usize) {
    // A thread being torn down has no stack left to put back.
    _ = STACK.try_with(|stack| {
        frame.ended_on.set(ptr::from_ref(stack).addr());
        stack.current.set(outer);
        if outer.is_null() {
            stack.cancelling.set(false);
        }
        stack.below.set(outer_below);
        if held > stack.peak.get() {
            stack.peak.set(held);
        }
    });
}

/// Runs `f` with the frame of the innermost [`task`] the calling thread is in: for code a task
/// reaches through calls that don't pass its frame (a query's body, run by the db's executor).
/// Stops, naming the caller, when it is in none: code that allocates for a task runs in one
/// (no-fallback). What `f` returns cannot borrow the frame.
#[track_caller]
pub fn current<R>(f: impl FnOnce(&Frame) -> R) -> R {
    // SAFETY: the frame is used only within `f`, called here, inside the task's closure.
    let frame = unsafe { innermost() };
    let Some(frame) = frame else { panic!("frame::current outside any frame::task") };
    f(frame)
}

/// The calling thread's innermost task frame, taken off it for a job that switches out (a
/// fiber's, which may resume on another thread): the thread is in no task after, until
/// [`put_current`] puts back what this took, on the thread the job resumes on. With
/// [`take_below`] and [`take_cancelling`], the rest of the job's task state.
#[inline(never)]
pub fn take_current() -> usize {
    let taken = STACK.try_with(|stack| stack.current.replace(ptr::null())).unwrap_or(ptr::null());
    check_carried(taken, "switched out");
    taken.expose_provenance()
}

/// Puts back what [`take_current`] took, on a thread in no task.
///
/// # Panics
///
/// Where the thread is in a task: another job's frame would be lost.
#[inline(never)]
pub fn put_current(taken: usize) {
    let frame: *const Frame = ptr::with_exposed_provenance(taken);
    check_carried(frame, "switched in");
    STACK.with(|stack| {
        assert!(stack.current.get().is_null(), "a job's frame put back on a thread inside another task");
        // Used only as `innermost` says: by the job that owns the frame, which runs on this
        // thread from now on.
        stack.current.set(frame);
    });
}

/// What the frames enclosing the switching job's task used ([`take_current`]).
#[inline(never)]
pub fn take_below() -> usize {
    STACK.try_with(|stack| stack.below.replace(0)).unwrap_or(0)
}

#[inline(never)]
pub fn put_below(taken: usize) {
    STACK.with(|stack| stack.below.set(taken));
}

/// Whether the switching job is unwinding from [`cancel`] ([`take_current`]).
#[inline(never)]
pub fn take_cancelling() -> usize {
    STACK.try_with(|stack| usize::from(stack.cancelling.replace(false))).unwrap_or(0)
}

#[inline(never)]
pub fn put_cancelling(taken: usize) {
    STACK.with(|stack| stack.cancelling.set(taken != 0));
}

/// Whether the calling thread is in a [`task`], so [`current`] has a frame to give.
#[inline(never)]
pub fn in_task() -> bool {
    STACK.try_with(|stack| !stack.current.get().is_null()).unwrap_or(false)
}

/// Unwinds the calling task with [`Cancelled`], for its scheduler to run again: what the cap does
/// to the newest waiter when every holder waits. Until the thread's outermost task ends,
/// [`cancelling`] says so, for the guards the unwind drops on its way out (a query's claim,
/// released to its waiters as a cancel, not a panic).
#[track_caller]
pub fn cancel() -> ! {
    assert!(in_task(), "frame::cancel outside any frame::task");
    _ = STACK.try_with(|stack| stack.cancelling.set(true));
    // alloc: a cancel's payload, once per cancelled task
    std::panic::resume_unwind(Box::new(Cancelled))
}

/// Whether the calling thread is unwinding from [`cancel`].
#[inline(never)]
pub fn cancelling() -> bool {
    STACK.try_with(|stack| stack.cancelling.get()).unwrap_or(false)
}

/// Runs `f` and says the most bytes the calling thread's task frames held at once while it ran,
/// above what they held when it began: a nested task's on top of its enclosing tasks'. Measures
/// nest, an inner one's peak part of the outer one's. Frames made with [`Frame::new`] are not
/// counted.
///
/// It is counted when a task ends and when a measure ends, never per allocation, and allocates
/// nothing. An enclosing frame that grows while an inner task runs counts its growth on top of
/// the inner task's peak, so the measure errs high, never low.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, usize) {
    struct Restore<'f> {
        frame: Option<&'f Frame>,
        below: usize,
        frame_peak: usize,
        thread_peak: usize,
    }
    impl Restore<'_> {
        /// The most held since the measure began, as the thread stands now.
        fn top(&self) -> usize {
            let inner = self.frame.map_or(0, |frame| self.below + frame.peak());
            thread_peak().max(inner)
        }
    }
    impl Drop for Restore<'_> {
        // On a panic too: the outer measure and the frame's own peak still see what `f` held.
        fn drop(&mut self) {
            let top = self.top();
            if let Some(frame) = self.frame {
                frame.peak.set(frame.peak().max(self.frame_peak));
            }
            set_thread_peak(top.max(self.thread_peak));
        }
    }
    // SAFETY: the frame is used only in this call, made inside its task's closure; the tasks `f`
    // runs end before it returns, so it is the innermost again when `Restore` reads it.
    let frame = unsafe { innermost() };
    let below = STACK.try_with(|stack| stack.below.get()).unwrap_or(0);
    let base = below + frame.map_or(0, Frame::used);
    let frame_peak = frame.map_or(0, |frame| {
        let peak = frame.peak();
        frame.peak.set(frame.used());
        peak
    });
    let thread_peak = STACK.try_with(|stack| stack.peak.replace(base)).unwrap_or(0);
    let restore = Restore { frame, below, frame_peak, thread_peak };
    let result = f();
    let top = restore.top();
    drop(restore);
    (result, top.saturating_sub(base))
}

#[inline(never)]
fn thread_peak() -> usize {
    STACK.try_with(|stack| stack.peak.get()).unwrap_or(0)
}

#[inline(never)]
fn set_thread_peak(peak: usize) {
    _ = STACK.try_with(|stack| stack.peak.set(peak));
}

/// Blocks of frame memory (chunks and spans) the calling thread keeps warm for its next tasks.
pub fn spare_frames() -> usize {
    SHELF_OF.try_with(Shelf::count).unwrap_or(0)
}

/// Trims the frame memory the calling thread keeps warm to `keep` bytes, the rest given back
/// to the kernel: what a worker does when it goes idle after a burst.
pub fn trim_spare(keep: usize) {
    _ = SHELF_OF.try_with(|shelf| shelf.trim(keep));
}

/// Bytes the calling thread's task frames hold of the [`Cap`] now.
pub fn capped() -> u64 {
    cap::thread_holding()
}

/// The items of `items` in a vector in `frame`, its room the iterator's lower bound to start:
/// `bumpalo`'s `Vec::from_iter_in`.
#[inline]
pub fn collect<'f, T>(frame: &'f Frame, items: impl IntoIterator<Item = T>) -> Vec<T, &'f Frame> {
    let items = items.into_iter();
    let mut out = Vec::with_capacity_in(items.size_hint().0, frame);
    out.extend(items);
    out
}

const _: () = assert!(mem::size_of::<Marker>() == 16);

#[cfg(test)]
mod tests {
    use std::{mem::MaybeUninit, panic};

    use super::*;

    /// What `task` panics with when this thread's stack names `stale` as the enclosing frame.
    fn started_inside(stale: *const Frame) -> String {
        let before = STACK.with(|stack| stack.current.replace(stale));
        let got = panic::catch_unwind(|| task(Tag::Other, |_| ()));
        STACK.with(|stack| stack.current.set(before));
        let payload = got.expect_err("a task inside a frame that is not live panics");
        payload.downcast_ref::<String>().cloned().expect("the panic's text is formatted")
    }

    #[test]
    fn a_task_inside_a_dropped_frame_panics_naming_the_frame_its_owner_and_the_task() {
        let mut slot = MaybeUninit::new(Frame::empty(FRAME_RESERVE, Tag::Other, 7, true));
        let stale: *const Frame = slot.as_ptr();
        // SAFETY: the frame was written just above and is dropped once; its bytes stay, as a
        // fiber's stack's do after the task on it ends.
        unsafe { slot.assume_init_drop() };
        let text = started_inside(stale);
        assert!(text.contains(&format!("inside the frame at {stale:p}")), "{text}");
        assert!(text.contains("it is dropped: its seal is 0xdeadf4a3deadf4a3"), "{text}");
        assert!(text.contains("its owner, task 7,"), "{text}");
        assert!(text.contains("blaze-mem: task "), "{text}");
        assert!(text.contains("Task 7 was opened at ") && text.contains("frame.rs:"), "{text}");
        assert!(text.contains("and ended by no task's end."), "{text}");
    }

    #[test]
    fn a_task_inside_memory_that_never_was_a_frame_panics_naming_it() {
        let junk = [0u64; 64];
        assert!(mem::size_of::<Frame>() <= mem::size_of_val(&junk));
        let stale = junk.as_ptr().cast::<Frame>();
        let text = started_inside(stale);
        assert!(text.contains("it is not a frame, or overwritten: its seal is 0x0"), "{text}");
    }

    #[test]
    fn a_task_inside_a_small_number_taken_for_a_frame_panics_before_reading_it() {
        for at in [0x8usize, 0x40, LOWEST - 1] {
            let text = started_inside(ptr::without_provenance(at));
            assert!(text.contains("is below the lowest address mapped"), "{text}");
        }
        let junk = [0u64; 64];
        let skewed = junk.as_ptr().cast::<u8>().wrapping_add(1).cast::<Frame>();
        assert!(started_inside(skewed).contains("is not aligned as a frame is"));
    }

    #[test]
    fn a_dropped_frame_carried_across_a_switch_fails_at_the_switch() {
        let mut slot = MaybeUninit::new(Frame::empty(FRAME_RESERVE, Tag::Other, 9, true));
        let stale: *const Frame = slot.as_ptr();
        // SAFETY: as in the test of a task inside a dropped frame.
        unsafe { slot.assume_init_drop() };
        let taken = stale.expose_provenance();
        let got = panic::catch_unwind(|| put_current(taken));
        let payload = got.expect_err("a dropped frame put back on a thread panics");
        let text = payload.downcast_ref::<String>().cloned().expect("the panic's text is formatted");
        assert!(text.contains(&format!("a job switched in carrying the frame at {stale:p}")), "{text}");
        assert!(text.contains("it is dropped; the frame's address is "), "{text}");
        assert!(!in_task(), "the refused frame is not made current");
    }

    #[test]
    fn a_live_frame_carried_across_a_switch_is_current_again() {
        task(Tag::Other, |frame| {
            let at: *const Frame = frame;
            let taken = take_current();
            assert!(!in_task());
            put_current(taken);
            assert_eq!(STACK.with(|stack| stack.current.get()), at);
        });
    }

    #[test]
    fn a_task_inside_a_live_task_starts_in_its_frame() {
        let inner = task(Tag::Other, |outer| {
            let at: *const Frame = outer;
            task(Tag::Other, |_| STACK.with(|stack| stack.current.get()) != at)
        });
        assert!(inner, "the inner task's frame is current inside it");
    }
}
