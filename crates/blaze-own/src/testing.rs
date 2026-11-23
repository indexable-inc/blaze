//! Allocation-failure and leak testing, after Zig's `std.testing.allocator`, `FailingAllocator`
//! and `checkAllAllocationFailures` (the `testing` feature; `no_std`, no allocation of its own).
//!
//! - [`Counting`] wraps an allocator and keeps every live block: its sequence number, size and
//!   alignment. [`Counting::check`] says what is still live (a leak) and what was freed wrong (a
//!   double free, a free with another layout); fresh blocks, and the new bytes of a grown one,
//!   are filled with [`POISON`], so a read of a slot never written reads defined, recognizable
//!   bytes. Growth and shrinking are passed to the inner allocator, so one that resizes in
//!   place (an [`Inline`](crate::Inline) buffer's newest block) still does.
//! - [`FailAt`] wraps an allocator and refuses its `n`-th allocation or growth (one refusal).
//! - [`every_failure`] runs a closure once to count its allocations and growths, then once per
//!   one of them with that one refused, and requires every run to come back with the closure's
//!   typed error and to leave nothing live. A panic fails the test that called it, and an abort
//!   ends it: both are findings, as the rule for library code is a typed error. Under Miri it
//!   refuses [`MIRI_FAILURE_POINTS`] of them, spread evenly over the run.
//!
//! The table of live blocks has [`SLOTS`] entries (rule 18, a table a cap): an allocation past
//! it is refused and recorded as wrong, never untracked.
//!
//! How blaze-own's own methods fail, which these tests hold it to: every `try_` method returns
//! an [`AllocFailure`](crate::AllocFailure) (or a [`Refused`](crate::Refused) carrying the
//! value back) and leaves the value as it was. The methods without `try_` (`push`, `reserve`,
//! `new_in` of a box or counted value, `insert`, `extend`, `clone`, ...) call `error::fail`,
//! which panics with the `AllocFailure`: a panic, not an abort, so it fails one job (rule 20).
//! Only a reference count about to pass `isize::MAX` aborts. So library code uses the `try_`
//! forms, and only a binary's top decides that a refusal stops the process.

use core::{
    alloc::{AllocError, Allocator, Layout},
    fmt,
    ptr::NonNull,
    sync::atomic::{
        AtomicU8, AtomicUsize,
        Ordering::{AcqRel, Acquire, Relaxed, Release},
    },
};

/// A test's root allocator capability: the process's [`Heap`](crate::Heap), for a test, which
/// is its own entry (decision D102). The one constructor of a heap for tests, so the only name
/// a test uses: `blaze_own::testing::heap().alloc()` where code took `Alloc::global()`, and
/// `blaze_own::testing::heap().shared()` where it took `SharedAlloc::global()`; a crate turns
/// it on in its dev-dependencies (`blaze-own = { workspace = true, features = ["testing"] }`).
/// A use outside test code is a bug: the ratchet (tools/ratchets/alloc-global.txt) counts it
/// as an ambient allocator, as it counts `Alloc::global()`; non-test code takes `&Heap` or an
/// `Alloc<'_>` from its caller, up to the program's entry ([`Heap::process`](crate::Heap::process)).
pub fn heap() -> &'static crate::Heap {
    crate::Heap::for_tests()
}

/// The test heap as an allocator TYPE that is `Default`: for a test that must name an
/// allocator through a bound that defaults it (blaze-hash's `Default` and `FromIterator` for its
/// maps and sets, `A: Allocator + Default`; a `mem::take` of a struct holding one). Zero bytes;
/// every call goes to [`heap`]'s shared handle, so a block made through it may be freed through
/// any handle of the process heap and the other way round. In this module only (the `testing`
/// feature): shipped code never names an allocator it was not handed, so it never defaults one
/// (D170 C1); it takes `A` and calls `new_in(alloc)`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct TestHeap;

// SAFETY: every method forwards, unchanged, to the process heap's shared handle, whose own
// contract (`SharedAlloc`'s `Allocator`) it is; every `TestHeap` is that one allocator.
unsafe impl Allocator for TestHeap {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        heap().shared().allocate(layout)
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        heap().shared().allocate_zeroed(layout)
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract for `deallocate`, passed on; the block is the heap's.
        unsafe { heap().shared().deallocate(ptr, layout) }
    }

    #[inline]
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract for `grow`, passed on.
        unsafe { heap().shared().grow(ptr, old, new) }
    }

    #[inline]
    unsafe fn grow_zeroed(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract for `grow_zeroed`, passed on.
        unsafe { heap().shared().grow_zeroed(ptr, old, new) }
    }

    #[inline]
    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract for `shrink`, passed on.
        unsafe { heap().shared().shrink(ptr, old, new) }
    }
}

// SAFETY: every `TestHeap` is the one process heap (above), so a copy frees what another made.
unsafe impl core::alloc::AllocatorClone for TestHeap {}

/// A process-lifetime bump allocator for `'static` test fixtures in crates that cannot depend
/// on `blaze-mem` or write `unsafe` (`#![forbid(unsafe_code)]`). Allocations live for the life
/// of the test process (`deallocate` is a no-op), satisfying [`BumpAlloc`](crate::BumpAlloc)
/// for `'static`. Available under the `testing` feature only; production code takes a
/// [`Bump`](crate::Bump) backed by a [`Frame`](../../blaze_mem) or arena.
struct TestBump;

// SAFETY: forwards allocations to the process test heap; `deallocate` is a no-op so blocks
// remain valid for `'static`.
unsafe impl Allocator for TestBump {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        heap().shared().allocate(layout)
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        heap().shared().allocate_zeroed(layout)
    }

    #[inline]
    unsafe fn deallocate(&self, _ptr: NonNull<u8>, _layout: Layout) {}
}

// SAFETY: `deallocate` is a no-op and `TEST_BUMP` lives for `'static`, so dropping a
// container allocated from `TestBump` without running `deallocate` leaves its bytes valid for
// `'static`.
unsafe impl crate::BumpAlloc for TestBump {}

/// A `'static` [`Bump`](crate::Bump) handle for test fixtures (under the `testing` feature).
pub fn bump() -> crate::Bump<'static> {
    static BUMP: crate::Erased<TestBump> = crate::Erased::new(TestBump);
    BUMP.bump()
}

/// The byte a fresh block is filled with.
pub const POISON: u8 = 0xA5;

/// How many blocks a [`Counting`] can hold live at once.
pub const SLOTS: usize = 256;

/// A slot's address word: free, being filled, or a live block's address.
const FREE: usize = 0;
const CLAIMED: usize = 1;

/// One entry of the live-block table.
struct Slot {
    /// [`FREE`], [`CLAIMED`] (fields being written), or the block's address (fields readable,
    /// published by a release store).
    addr: AtomicUsize,
    size: AtomicUsize,
    align: AtomicUsize,
    seq: AtomicUsize,
}

impl Slot {
    const EMPTY: Slot = Slot {
        addr: AtomicUsize::new(FREE),
        size: AtomicUsize::new(0),
        align: AtomicUsize::new(0),
        seq: AtomicUsize::new(0),
    };
}

/// What a [`Counting`] saw done wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Wrong {
    /// A free of a block not live: freed twice, or never this allocator's.
    NotLive = 1,
    /// A free (or a resize) with another layout than the block was made with.
    OtherLayout = 2,
    /// A request for zero bytes, which blaze-own never makes.
    ZeroSize = 3,
    /// More than [`SLOTS`] blocks live at once; the allocation was refused.
    TableFull = 4,
}

impl Wrong {
    fn from_code(code: u8) -> Option<Wrong> {
        match code {
            1 => Some(Wrong::NotLive),
            2 => Some(Wrong::OtherLayout),
            3 => Some(Wrong::ZeroSize),
            4 => Some(Wrong::TableFull),
            _ => None,
        }
    }
}

/// A block still live: its sequence number among the allocator's allocations (from 0), size
/// and alignment. No address: it differs between runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub seq: usize,
    pub size: usize,
    pub align: usize,
}

/// What [`Counting::check`] refused: blocks still live (the first few named) or something done
/// wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaked {
    pub live: usize,
    pub live_bytes: usize,
    /// The live blocks of lowest sequence number, up to four.
    pub first: [Option<Block>; 4],
    /// How many wrong things were seen, and the first.
    pub wrong: usize,
    pub first_wrong: Option<Wrong>,
}

impl fmt::Display for Leaked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(wrong) = self.first_wrong {
            write!(f, "{} wrong uses of the allocator, the first {wrong:?}", self.wrong)?;
            if self.live == 0 {
                return Ok(());
            }
            f.write_str("; ")?;
        }
        write!(f, "{} blocks ({} bytes) still live", self.live, self.live_bytes)?;
        for block in self.first.iter().flatten() {
            write!(f, ", allocation #{} of {} bytes aligned to {}", block.seq, block.size, block.align)?;
        }
        Ok(())
    }
}

/// An allocator over `A` that keeps every live block (see the module).
pub struct Counting<A: Allocator> {
    inner: A,
    slots: [Slot; SLOTS],
    /// Slots below this have been used; the rest are free. Scans stop here.
    high: AtomicUsize,
    allocations: AtomicUsize,
    frees: AtomicUsize,
    live: AtomicUsize,
    live_bytes: AtomicUsize,
    wrong: AtomicUsize,
    first_wrong: AtomicU8,
}

impl<A: Allocator> Counting<A> {
    pub const fn new(inner: A) -> Counting<A> {
        Counting {
            inner,
            slots: [Slot::EMPTY; SLOTS],
            high: AtomicUsize::new(0),
            allocations: AtomicUsize::new(0),
            frees: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            live_bytes: AtomicUsize::new(0),
            wrong: AtomicUsize::new(0),
            first_wrong: AtomicU8::new(0),
        }
    }

    /// Allocations and growths made so far: a grown block takes a new sequence number, so a
    /// leak names its last growth.
    pub fn allocations(&self) -> usize {
        self.allocations.load(Acquire)
    }

    pub fn frees(&self) -> usize {
        self.frees.load(Acquire)
    }

    /// Blocks live now.
    pub fn live(&self) -> usize {
        self.live.load(Acquire)
    }

    pub fn live_bytes(&self) -> usize {
        self.live_bytes.load(Acquire)
    }

    /// Calls `each` with every live block's address and layout, in table order.
    pub fn live_blocks(&self, mut each: impl FnMut(usize, Layout)) {
        for slot in &self.slots[..self.high.load(Acquire)] {
            let addr = slot.addr.load(Acquire);
            if addr > CLAIMED {
                let (size, align) = (slot.size.load(Relaxed), slot.align.load(Relaxed));
                if let Ok(layout) = Layout::from_size_align(size, align) {
                    each(addr, layout);
                }
            }
        }
    }

    /// Nothing live and nothing done wrong; else what is.
    pub fn check(&self) -> Result<(), Leaked> {
        let wrong = self.wrong.load(Acquire);
        let live = self.live();
        if wrong == 0 && live == 0 {
            return Ok(());
        }
        let mut first = [None; 4];
        for slot in &self.slots[..self.high.load(Acquire)] {
            if slot.addr.load(Acquire) <= CLAIMED {
                continue;
            }
            let block =
                Block { seq: slot.seq.load(Relaxed), size: slot.size.load(Relaxed), align: slot.align.load(Relaxed) };
            // Keep the four of lowest sequence number, in order.
            let mut carried = Some(block);
            for kept in &mut first {
                match (*kept, carried) {
                    (None, Some(_)) => {
                        *kept = carried;
                        carried = None;
                    }
                    (Some(old), Some(new)) if new.seq < old.seq => {
                        *kept = Some(new);
                        carried = Some(old);
                    }
                    _ => {}
                }
            }
        }
        Err(Leaked {
            live,
            live_bytes: self.live_bytes(),
            first,
            wrong,
            first_wrong: Wrong::from_code(self.first_wrong.load(Acquire)),
        })
    }

    /// [`Counting::check`], panicking with what is live or wrong.
    #[track_caller]
    pub fn assert_empty(&self) {
        if let Err(leaked) = self.check() {
            panic!("blaze_own::testing::Counting: {leaked}");
        }
    }

    fn record(&self, wrong: Wrong) {
        self.wrong.fetch_add(1, AcqRel);
        let _ = self.first_wrong.compare_exchange(0, wrong as u8, AcqRel, Acquire);
    }

    /// Writes a claimed slot's fields, then publishes `addr` (a block's, or [`FREE`]).
    fn publish(slot: &Slot, addr: usize, layout: Layout, seq: usize) {
        slot.size.store(layout.size(), Relaxed);
        slot.align.store(layout.align(), Relaxed);
        slot.seq.store(seq, Relaxed);
        // Release: the fields are written before the address says they are readable, and read
        // (by `claim`) before a free offers the slot again.
        slot.addr.store(addr, Release);
    }

    /// Enters a new block in the table; `false` when the table is full.
    fn enter(&self, addr: usize, layout: Layout, seq: usize) -> bool {
        for (at, slot) in self.slots.iter().enumerate() {
            // Acquire: the slot's last free (a release store of FREE) happens before its reuse.
            if slot.addr.compare_exchange(FREE, CLAIMED, Acquire, Relaxed).is_err() {
                continue;
            }
            Counting::<A>::publish(slot, addr, layout, seq);
            self.high.fetch_max(at + 1, AcqRel);
            self.live.fetch_add(1, AcqRel);
            self.live_bytes.fetch_add(layout.size(), AcqRel);
            return true;
        }
        false
    }

    /// Claims the slot of the live block at `addr` (its address word now [`CLAIMED`]): the
    /// slot, the layout the block has and its sequence number; `None` when it is not live.
    fn claim(&self, addr: usize) -> Option<(&Slot, Layout, usize)> {
        if addr <= CLAIMED {
            return None;
        }
        for slot in &self.slots[..self.high.load(Acquire)] {
            // Acquire: pairs with `publish`'s release store, so the fields read are the block's.
            if slot.addr.compare_exchange(addr, CLAIMED, Acquire, Relaxed).is_err() {
                continue;
            }
            let (size, align) = (slot.size.load(Relaxed), slot.align.load(Relaxed));
            // SAFETY: the fields were written from a `Layout` (`publish`).
            let layout = unsafe { Layout::from_size_align_unchecked(size, align) };
            return Some((slot, layout, slot.seq.load(Relaxed)));
        }
        None
    }

    /// A block taken out of the table: its slot freed and the counts lowered.
    fn forget(&self, slot: &Slot, made: Layout) {
        Counting::<A>::publish(slot, FREE, made, 0);
        self.live.fetch_sub(1, AcqRel);
        self.live_bytes.fetch_sub(made.size(), AcqRel);
    }

    /// The claimed block, resized by `inner` (`resize`) to `new`, back in its slot: with a fresh
    /// sequence number and poisoned new bytes when it grew. Refused, with nothing passed on,
    /// when `old` is not the layout it was made with.
    fn resize(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
        grew: bool,
        resize: impl FnOnce(&A, Layout) -> Result<NonNull<[u8]>, AllocError>,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let Some((slot, made, seq)) = self.claim(ptr.addr().get()) else {
            self.record(Wrong::NotLive);
            return Err(AllocError);
        };
        if made != old {
            self.record(Wrong::OtherLayout);
            Counting::<A>::publish(slot, ptr.addr().get(), made, seq);
            return Err(AllocError);
        }
        match resize(&self.inner, made) {
            Err(refused) => {
                Counting::<A>::publish(slot, ptr.addr().get(), made, seq);
                Err(refused)
            }
            Ok(block) => {
                let seq = if grew { self.allocations.fetch_add(1, AcqRel) } else { seq };
                let start = block.cast::<u8>();
                Counting::<A>::publish(slot, start.addr().get(), new, seq);
                self.live_bytes.fetch_sub(made.size(), AcqRel);
                self.live_bytes.fetch_add(new.size(), AcqRel);
                if grew {
                    // SAFETY: the block has `new.size()` bytes; those past `made.size()` are new.
                    unsafe { start.as_ptr().add(made.size()).write_bytes(POISON, new.size() - made.size()) };
                }
                Ok(NonNull::slice_from_raw_parts(start, new.size()))
            }
        }
    }
}

// SAFETY: blocks come from `inner` for the layout asked; a free, growth or shrink is passed to
// `inner` only for a live block with the layout it was made with (anything else is recorded
// and not passed on), and the table follows each block to where `inner` moved it.
unsafe impl<A: Allocator> Allocator for Counting<A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            self.record(Wrong::ZeroSize);
            return Err(AllocError);
        }
        let block = self.inner.allocate(layout)?;
        let addr = block.cast::<u8>().addr().get();
        let seq = self.allocations.fetch_add(1, AcqRel);
        if !self.enter(addr, layout, seq) {
            self.record(Wrong::TableFull);
            // SAFETY: just allocated from `inner` with `layout`, never handed out.
            unsafe { self.inner.deallocate(block.cast(), layout) };
            return Err(AllocError);
        }
        // SAFETY: the block has at least `layout.size()` bytes, just allocated and unused.
        unsafe { block.cast::<u8>().as_ptr().write_bytes(POISON, layout.size()) };
        Ok(NonNull::slice_from_raw_parts(block.cast(), layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        match self.claim(ptr.addr().get()) {
            None => self.record(Wrong::NotLive),
            Some((slot, made, _seq)) => {
                if made != layout {
                    self.record(Wrong::OtherLayout);
                }
                self.forget(slot, made);
                self.frees.fetch_add(1, AcqRel);
                // SAFETY: the block was live and `inner`'s, made with `made`; it is out of the
                // table, so it is freed once.
                unsafe { self.inner.deallocate(ptr, made) };
            }
        }
    }

    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: `ptr` is a live block of `inner` made with `made` (checked by `resize`), and
        // `new` is at least as large (the caller's contract).
        self.resize(ptr, old, new, true, |inner, made| unsafe { inner.grow(ptr, made, new) })
    }

    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: `ptr` is a live block of `inner` made with `made` (checked by `resize`), and
        // `new` is at most as large (the caller's contract).
        self.resize(ptr, old, new, false, |inner, made| unsafe { inner.shrink(ptr, made, new) })
    }
}

/// An allocator over `A` that refuses its `n`-th allocation or growth, counted from 0 (one
/// refusal; then it grants again). Shrinks and frees are passed on uncounted.
pub struct FailAt<A: Allocator> {
    inner: A,
    /// The index of the event to refuse; `usize::MAX` for none.
    at: AtomicUsize,
    seen: AtomicUsize,
}

impl<A: Allocator> FailAt<A> {
    /// Refuses the allocation or growth numbered `n`.
    pub const fn at(inner: A, n: usize) -> FailAt<A> {
        FailAt { inner, at: AtomicUsize::new(n), seen: AtomicUsize::new(0) }
    }

    /// Refuses nothing until [`FailAt::fail_at`] says.
    pub const fn never(inner: A) -> FailAt<A> {
        FailAt::at(inner, usize::MAX)
    }

    /// From now on, refuses the event numbered `n` (as [`FailAt::seen`] counts) instead.
    pub fn fail_at(&self, n: usize) {
        self.at.store(n, Release);
    }

    /// Allocations and growths asked for so far, refused ones included.
    pub fn seen(&self) -> usize {
        self.seen.load(Acquire)
    }

    pub fn inner(&self) -> &A {
        &self.inner
    }

    /// Counts one event; whether it is the one to refuse.
    fn refuses(&self) -> bool {
        self.seen.fetch_add(1, AcqRel) == self.at.load(Acquire)
    }
}

// SAFETY: every block is `inner`'s, passed through unchanged; a refusal returns before asking.
unsafe impl<A: Allocator> Allocator for FailAt<A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if self.refuses() {
            return Err(AllocError);
        }
        self.inner.allocate(layout)
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if self.refuses() {
            return Err(AllocError);
        }
        self.inner.allocate_zeroed(layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.inner.deallocate(ptr, layout) }
    }

    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if self.refuses() {
            return Err(AllocError);
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { self.inner.grow(ptr, old, new) }
    }

    unsafe fn grow_zeroed(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if self.refuses() {
            return Err(AllocError);
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { self.inner.grow_zeroed(ptr, old, new) }
    }

    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.inner.shrink(ptr, old, new) }
    }
}

/// What [`every_failure`] refused.
#[derive(Debug)]
pub enum EveryFailure<E> {
    /// The run with nothing refused failed.
    CleanRunFailed(E),
    /// With event `at` refused, the run succeeded anyway: the refusal was swallowed (rule 3).
    Swallowed { at: usize },
    /// With event `at` refused, the run asked for only `seen` events, so the refusal never
    /// happened: the closure does not allocate the same way each time.
    NotReached { at: usize, seen: usize },
    /// A run (`at` refused, or `None` for the clean run) left blocks live or used the allocator
    /// wrong.
    Leaked { at: Option<usize>, leaked: Leaked },
}

impl<E: fmt::Display> fmt::Display for EveryFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EveryFailure::CleanRunFailed(error) => write!(f, "the run with nothing refused failed: {error}"),
            EveryFailure::Swallowed { at } => {
                write!(f, "with allocation or growth #{at} refused the run still succeeded: the refusal was swallowed")
            }
            EveryFailure::NotReached { at, seen } => write!(
                f,
                "allocation or growth #{at} was to be refused, but the run asked for only {seen}: it does not allocate the same way each run"
            ),
            EveryFailure::Leaked { at: Some(at), leaked } => {
                write!(f, "with allocation or growth #{at} refused: {leaked}")
            }
            EveryFailure::Leaked { at: None, leaked } => write!(f, "with nothing refused: {leaked}"),
        }
    }
}

/// Failure points [`every_failure`] tries under Miri at most. Each point reruns the whole
/// closure, so the cost is the square of its size, which the interpreter cannot pay.
pub const MIRI_FAILURE_POINTS: usize = 24;

/// The allocations and growths to refuse, one run each: all `events` of them, or under Miri
/// [`MIRI_FAILURE_POINTS`] spread evenly from the first to the last, so the early growth, the
/// middle and the last allocation are each still refused once.
fn failure_points(events: usize) -> impl Iterator<Item = usize> {
    let tried = if cfg!(miri) { events.min(MIRI_FAILURE_POINTS) } else { events };
    (0..tried).map(move |i| if tried == events { i } else { i * (events - 1) / (tried - 1) })
}

/// Runs `run` once with nothing refused, counting its allocations and growths, then once per
/// one of them with that one refused (see the module; under Miri, [`MIRI_FAILURE_POINTS`] of
/// them spread evenly); the number of events, or the first run that broke the rule.
///
/// `run` gets the allocator to build with; what it returns cannot borrow it, so everything it
/// made is dropped by the time it returns, and anything still live is a leak.
pub fn every_failure<'b, B: Allocator, T, E>(
    base: &'b B,
    mut run: impl FnMut(&FailAt<Counting<&'b B>>) -> Result<T, E>,
) -> Result<usize, EveryFailure<E>> {
    let clean = FailAt::never(Counting::new(base));
    let result = run(&clean);
    let events = clean.seen();
    let result = result.map(drop);
    clean.inner().check().map_err(|leaked| EveryFailure::Leaked { at: None, leaked })?;
    result.map_err(EveryFailure::CleanRunFailed)?;
    for at in failure_points(events) {
        let alloc = FailAt::at(Counting::new(base), at);
        let result = run(&alloc);
        let seen = alloc.seen();
        let ok = result.is_ok();
        drop(result);
        alloc.inner().check().map_err(|leaked| EveryFailure::Leaked { at: Some(at), leaked })?;
        if seen <= at {
            return Err(EveryFailure::NotReached { at, seen });
        }
        if ok {
            return Err(EveryFailure::Swallowed { at });
        }
    }
    Ok(events)
}
