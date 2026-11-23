//! Memory is explicit (docs/v2/plan/design/std.md, "Memory is explicit"): what bstd hands back
//! is made in an allocator its caller handed in.
//!
//! | allocator | from | lives |
//! |---|---|---|
//! | [`Frame`] | `cx.frame()` | until the task ends |
//! | [`Arena`] | `mem.arena("name", limit)` | until dropped |
//!
//! Both are blaze-mem's [`Frame`], the bump allocator of blaze's tasks, which already keeps a
//! limit (`Frame::with_reserve`): an arena is a named frame whose reserve is its limit, so
//! nothing here is a new allocator and nothing needs `unsafe`. Running out is an outcome:
//! [`Bytes::try_extend`] returns [`Stopped::OutOfMemory`] naming the arena. A collection the
//! caller makes in an arena itself (`Vec::new_in(arena.alloc())`) must grow through its
//! `try_` forms (`try_reserve`): an infallible growth past the limit is `alloc`'s allocation
//! failure, until bstd's own collections (S4) turn it into the task's failure.
//!
//! The rest of blaze-mem's allocators by lifetime are re-exported here as they are: a
//! request's arena ([`RequestArena`], blaze-mem's `Arena`, named so beside bstd's own
//! [`Arena`]), pools and their caches, slabs with generational handles, revisions, regions and
//! the vectors over them, kept blocks, the subsystem [`Tag`]s, and owned text in an allocator
//! ([`String`], [`format_in!`]). The functions of blaze-mem that reach a frame through the
//! thread (`frame::current`, `frame::task`) are left out: in bstd the frame is `cx.frame()`.
//!
//! # The allocator a function takes
//!
//! | form | reach for it instead of | cost |
//! |---|---|---|
//! | [`Alloc`] | `<A: Allocator>(alloc: A)` on a function, or the global heap | one word, `Copy`; a call through it is two loads and an indirect call; the function is compiled once |
//! | [`Erased`] | an allocator with no header of its own | its header and the allocator: `Erased::new(cx.frame()).handle()` is an [`Alloc`] |
//! | [`SharedAlloc`] | an [`Alloc`] in a value that crosses threads | the same word, made only from a `Sync` allocator; `shared.local()` is an [`Alloc`] |
//!
//! These are blaze-own's handles. They are here, not in a `bstd::alloc`, because bstd's root
//! links `alloc` as `extern crate alloc`, whose name a module of the root cannot take.
//! `Alloc::global()` is not a bstd path: a program's allocator is handed down from its entry.
//!
//! # The heap is a grant
//!
//! Memory that outlives every task (a `'static` store, a pinned box) is the process's heap,
//! [`Heap`]: a `main` or a `#[bstd::test]` names it in its signature (`heap: Heap`) and its
//! entry hands it over, as it hands `out: Out` (D120). Nothing else makes one: bstd's entry
//! takes the process's heap in one function (`entry::process_heap`) and nowhere else, so a
//! function that allocates for the long term says so by taking a `Heap` (or the [`Alloc`] it
//! gives) from its caller.

use core::{
    alloc::{AllocError, Allocator, AllocatorClone, Layout, StaticAllocator},
    ptr::NonNull,
};

pub use blaze_own::{
    Alloc, BBox, BString, BVec, Bump, BumpAlloc, CloneIn, DefaultIn, Erased, FromIn, FromStrIn,
    IntoIn, LocalShare, ParseIn, Share, SharedAlloc, ToOwnedIn,
};
pub use blaze_own::pod::{
    self, AnyBitPattern, CastError, CheckedBitPattern, NoUninit, Pod, TransparentWrapper, Zeroable,
};
pub use blaze_mem::{
    AppendVec, ArenaLocal, Block, Built, Cache, Column, Frame, FromUtf8Error, HUGE_PAGE, Handle,
    Kept, Marker, MemError, Pool, Pooled, Region, RegionVec, Revisions, Slab, String, Tag,
    format_in, page_size,
};
pub use blaze_mem::Arena as RequestArena;
pub mod fast;
pub use fast::{
    GenMap, KindSet, SmallVec, interp_search_by_prefix, simd_count_byte, simd_find_byte_any4,
    simd_mismatch, simd_prefix_sum_u32,
};

use alloc::vec::Vec;
use crate::{Plant, Stopped, io::Io};

/// The most a frame not given a limit hands out: a task's ([`crate::task`]) and the root's.
pub const TASK_FRAME: usize = 64 << 20;

/// The process's heap, as a grant (D120): one word, `Copy`; each call goes to the global
/// allocator through blaze-own's handle (two loads and an indirect call). What `main` or a
/// `#[bstd::test]` names as `heap: Heap`; it fills itself ([`crate::grant::Kind::Heap`]).
///
/// It is the allocator for what lives as long as the program: a `'static` store
/// (`heap.alloc()`, an `Alloc<'static>`), a value that crosses threads (`heap.shared()`), and
/// a pinned box (`Box::pin_in(future, heap)`), which needs an allocator that never reuses a
/// live block ([`StaticAllocator`]): a frame or an arena is not one, since either can be reset
/// or dropped under a box that was forgotten. Everything shorter lives in `cx.frame()`.
#[derive(Clone, Copy)]
pub struct Heap {
    heap: &'static blaze_own::Heap,
}

impl Heap {
    /// The grant over `heap`: made only by bstd's entries (`entry::process_heap`), so holding
    /// one is the proof it was handed down.
    pub(crate) const fn of(heap: &'static blaze_own::Heap) -> Heap {
        Heap { heap }
    }

    /// The heap's one-word handle, for a function that takes an [`Alloc`].
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn alloc(self) -> Alloc<'static> {
        self.heap.alloc()
    }

    /// The heap's handle that may cross threads.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn shared(self) -> SharedAlloc<'static> {
        self.heap.shared()
    }

    /// blaze-own's heap this grant is, for an API that takes `&'static blaze_own::Heap` (a
    /// crate below bstd: blaze-fetch's `FetchEnv::heap`, blaze-cli's `request`).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn root(self) -> &'static blaze_own::Heap {
        self.heap
    }
}

impl core::fmt::Debug for Heap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Heap")
    }
}

// SAFETY: every method forwards to the heap's handle (`blaze_own::Heap::alloc`), the global
// allocator's, with the caller's arguments unchanged, so each method keeps that allocator's
// contract and the caller's part of it is passed on as it came. Every `Heap` is the same one
// global allocator, so a block made through one copy may be freed or resized through any other.
#[expect(unsafe_code, reason = "the heap's `Allocator`, forwarding to blaze-own's handle, with its SAFETY")]
unsafe impl Allocator for Heap {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.heap.alloc().allocate(layout)
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.heap.alloc().allocate_zeroed(layout)
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's: `ptr` was made by this allocator (the global one) with `layout`.
        unsafe { self.heap.alloc().deallocate(ptr, layout) }
    }

    #[inline]
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's, as `Allocator::grow` states it, for the same allocator.
        unsafe { self.heap.alloc().grow(ptr, old, new) }
    }

    #[inline]
    unsafe fn grow_zeroed(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's, as `Allocator::grow_zeroed` states it, for the same allocator.
        unsafe { self.heap.alloc().grow_zeroed(ptr, old, new) }
    }

    #[inline]
    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's, as `Allocator::shrink` states it, for the same allocator.
        unsafe { self.heap.alloc().shrink(ptr, old, new) }
    }
}

// SAFETY: a copy is the same global allocator (the one `blaze_own::Heap`), so a block made
// through either may be freed through the other; copying is cloning, and neither a copy nor a
// drop (it has none) touches a block.
#[expect(unsafe_code, reason = "the heap's copies are one allocator, with its SAFETY")]
unsafe impl AllocatorClone for Heap {}

// SAFETY: the process's heap never reuses a live block: the global allocator invalidates a
// block only when it is deallocated, grown or shrunk through it. `Heap` has no `Drop` and its
// reference is `'static`, so neither dropping it nor its lifetime ending frees anything; it has
// no reset and no safe method that frees (unlike a frame or an arena, which free every block at
// a reset or a drop); and every copy is the same allocator, so the same holds for all of them.
// That is what a pinned box needs: its value's memory is not reused before its drop runs, even
// when the box is forgotten.
#[expect(unsafe_code, reason = "the heap's promise that a live block is never reused, with its SAFETY")]
unsafe impl StaticAllocator for Heap {}

/// Bytes in an allocator its owner handed in: a frame's or an arena's.
pub struct Bytes<'a> {
    bytes: Vec<u8, &'a Frame>,
    arena: &'static str,
}

impl<'a> Bytes<'a> {
    /// Empty bytes in `frame`, which say they are `arena`'s when they run out of room.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn new_in(frame: &'a Frame, arena: &'static str) -> Bytes<'a> {
        Bytes { bytes: Vec::new_in(frame), arena }
    }

    /// Empty bytes in `frame` with room for at least `capacity` bytes reserved upfront, or
    /// [`Stopped::OutOfMemory`] if `frame` cannot fit it.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn with_capacity_in(frame: &'a Frame, arena: &'static str, capacity: usize) -> Result<Bytes<'a>, Stopped> {
        let mut out = Bytes::new_in(frame, arena);
        out.try_reserve(capacity)?;
        Ok(out)
    }

    /// Bytes this buffer can hold without growing.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// Mutable view of the bytes currently held.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Backing `Vec` for zero-memset direct kernel reads in `blaze-core` (keeps `blaze-std`
    /// 100% `#![deny(unsafe_code)]`).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn as_vec_mut(&mut self) -> &mut Vec<u8, &'a Frame> {
        &mut self.bytes
    }

    /// `more` added at the end, or [`Stopped::OutOfMemory`] with the bytes as they were.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn try_extend(&mut self, more: &[u8]) -> Result<(), Stopped> {
        self.try_reserve(more.len())?;
        self.bytes.extend_from_slice(more);
        Ok(())
    }

    /// Room for `more` bytes past the end, or [`Stopped::OutOfMemory`].
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn try_reserve(&mut self, more: usize) -> Result<(), Stopped> {
        self.bytes
            .try_reserve(more)
            .map_err(|_| Stopped::OutOfMemory { arena: self.arena, asked: more })
    }

    /// `more` zero bytes added at the end, lent back to be filled (by a
    /// [`crate::pending::Pending`] read), or [`Stopped::OutOfMemory`] with the bytes as they
    /// were.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn grow_zeroed(&mut self, more: usize) -> Result<&mut [u8], Stopped> {
        self.try_reserve(more)?;
        let at = self.bytes.len();
        self.bytes.resize(at + more, 0);
        Ok(&mut self.bytes[at..])
    }

    /// Only the first `len` bytes kept; nothing changes if it holds no more.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    /// The name its running out of room says.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn arena(&self) -> &'static str {
        self.arena
    }
}

impl core::fmt::Debug for Bytes<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Bytes({} of {})", self.bytes.len(), self.arena)
    }
}

/// Hands out named arenas, each with a limit: from [`Io::mem`].
#[derive(Clone)]
pub struct Mem {
    io: Io,
}

impl Mem {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(io: Io) -> Mem {
        Mem { io }
    }

    /// An arena named `name` that hands out at most `limit` bytes, alignment included.
    pub fn arena(&self, name: &'static str, limit: usize) -> Result<Arena, Stopped> {
        let reserve = match self.io.plant() {
            Plant::ArenaUnlimited => TASK_FRAME,
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::WaitIgnoresDeadline | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::UnsyncedRenameSyncs | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::ChanPassesCapacity | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates | Plant::AncestorsUnbounded => limit,
        };
        let frame = Frame::with_reserve(reserve, Tag::Other)
            .map_err(|_| Stopped::OutOfMemory { arena: name, asked: limit })?;
        Ok(Arena { name, limit, frame })
    }
}

/// Memory for a result that outlives its task: named, with a limit, freed when dropped.
/// Not linear: dropping it frees its memory and there is nothing else to finish, so leaving
/// one is not a mistake the checker should refuse.
pub struct Arena {
    name: &'static str,
    limit: usize,
    frame: Frame,
}

impl Arena {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn name(&self) -> &'static str {
        self.name
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes handed out so far, alignment included: never more than [`Arena::limit`].
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn used(&self) -> usize {
        self.frame.used()
    }

    /// Empty bytes in the arena.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn bytes(&self) -> Bytes<'_> {
        Bytes::new_in(&self.frame, self.name)
    }

    /// The arena's frame itself, its limit kept: for an owner that shares it between a
    /// collection and its elements ([`crate::eval`]'s generations).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn into_frame(self) -> Frame {
        self.frame
    }

    /// The allocator, for a collection of the caller's (`Vec::new_in(arena.alloc())`), grown
    /// through its `try_` forms. Implements [`BumpAlloc`], so containers can freeze directly into
    /// arena-lifetime slices via `.into_bump_slice()` / `.into_bump_str()`.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn alloc(&self) -> &Frame {
        &self.frame
    }

    /// Copies `text` into this arena, returning a `&mut str` reclaimed when the arena drops.
    #[blaze::nonblocking]
    pub fn alloc_str(&self, text: &str) -> &mut str {
        self.frame.alloc_str(text)
    }

    /// Copies `values` into this arena, returning a `&mut [T]` reclaimed when the arena drops.
    #[blaze::nonblocking]
    pub fn alloc_slice_copy<T: Copy>(&self, values: &[T]) -> &mut [T] {
        self.frame.alloc_slice_copy(values)
    }
}

impl core::fmt::Debug for Arena {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Arena({}: {} of {})", self.name, self.used(), self.limit)
    }
}
