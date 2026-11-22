//! The unbounded queue: a linked list of blocks of `CAP` slots.
//!
//! The head and the tail are each a position, an index and the block it is in. An index
//! counts slots in steps of `1 << SHIFT`, `LAP = CAP + 1` steps a block: offset `CAP` of a
//! lap is no slot but the moment a push installs the next block, which the others wait out.
//! The head's low bit (`HAS_NEXT`) says the head's block is not the last, so a pop need not
//! read the tail to know the queue is not empty.
//!
//! A slot's state is three bits: `WRITE` once its push wrote the value, `READ` once its pop
//! read it, `DESTROY` once the block's freeing reached it while it was still being read. The
//! pop of a block's last slot starts the free (block by block, readers finish in any order):
//! it walks the other slots, and the first it finds unread is marked `DESTROY` and left to
//! finish the free when its reader sets `READ`.

#[allow(blaze::std_use, reason = "the deprecated `SegQueue::new` and `Default` alone (the crate's doc)")]
use alloc::alloc::Global;
use core::{alloc::Allocator, cell::UnsafeCell, fmt, marker::PhantomData, mem::MaybeUninit, ptr};

use blaze_own::{
    Box,
    planted::{NoPlant, Planted},
};
use blaze_spin::SpinWait;

use crate::{
    Padded, QueuePlant,
    sync::{
        self,
        atomic::{self, AtomicPtr, AtomicUsize, Ordering},
    },
};

const WRITE: usize = 1;
const READ: usize = 2;
const DESTROY: usize = 4;

/// The head's bit that says its block is not the last.
const HAS_NEXT: usize = 1;
/// An index's steps are this far apart, below them the head's `HAS_NEXT`.
const SHIFT: usize = 1;

struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    state: AtomicUsize,
}

impl<T> Slot<T> {
    const EMPTY: Slot<T> =
        Slot { value: UnsafeCell::new(MaybeUninit::uninit()), state: AtomicUsize::new(0) };

    fn wait_write(&self) {
        SpinWait::until("a value a SegQueue push is writing", || {
            self.state.load(Ordering::Acquire) & WRITE != 0
        });
    }
}

struct Block<T, const CAP: usize> {
    next: AtomicPtr<Block<T, CAP>>,
    slots: [Slot<T>; CAP],
}

impl<T, const CAP: usize> Block<T, CAP> {
    /// A block of empty slots, in `alloc`.
    fn new_in<A: Allocator>(alloc: A) -> Box<Block<T, CAP>, A> {
        let block = Block { next: AtomicPtr::new(ptr::null_mut()), slots: [Slot::EMPTY; CAP] };
        Box::new_in(block, alloc)
    }

    /// The next block, which the push that filled this one's last slot is installing.
    fn wait_next(&self) -> *mut Block<T, CAP> {
        let mut next = ptr::null_mut();
        SpinWait::until("the next block a SegQueue push is linking", || {
            next = self.next.load(Ordering::Acquire);
            !next.is_null()
        });
        next
    }

    /// Frees `this` once every slot from `start` has been read, else leaves the first unread
    /// one marked to free it when its reader is done.
    ///
    /// # Safety
    ///
    /// `this` is a block every slot below `start` of which has been read, its last slot's
    /// pop has started its free, and no push reaches it again; it was allocated in `alloc`
    /// (the queue's allocator).
    unsafe fn destroy<A: Allocator>(
        this: *mut Block<T, CAP>,
        start: usize,
        alloc: A,
        plant: impl Planted<QueuePlant>,
    ) {
        let ask = !plant.has(QueuePlant::FreeUnasked);
        // The last slot is the one whose pop started this: it is read.
        if ask {
            for at in start..CAP - 1 {
                // SAFETY: the block is not freed yet: its free is this call's.
                let slot = unsafe { &(*this).slots[at] };
                // Not read yet: its reader frees the block when done.
                if slot.state.load(Ordering::Acquire) & READ == 0
                    && slot.state.fetch_or(DESTROY, Ordering::AcqRel) & READ == 0
                {
                    return;
                }
            }
        }
        // SAFETY: as the contract says, and every slot is read: this is the block's one free.
        let block = unsafe { Box::from_raw_in(this, alloc) };
        for slot in &block.slots {
            sync::freed(slot.value.get());
        }
        // The plant's free is the model's alone: the bytes stay, so what the model reports is
        // the read after it, not a crash of the test. (`ask` is always true under `NoPlant`.)
        if !ask {
            core::mem::forget(block);
            return;
        }
        drop(block);
    }
}

/// A position: an index and the block it is in.
struct Position<T, const CAP: usize> {
    index: AtomicUsize,
    block: AtomicPtr<Block<T, CAP>>,
}

/// An unbounded queue of `T` that any number of threads push to and pop from, in blocks of
/// `CAP` slots (a test's model uses a small one, so its executions reach a block's end), each
/// block in `A` (no default: D170 C1). Shared between threads, `A` is `Send + Sync` (a
/// `SharedAlloc`).
pub struct SegQueue<T, A: Allocator, const CAP: usize = 31> {
    head: Padded<Position<T, CAP>>,
    tail: Padded<Position<T, CAP>>,
    alloc: A,
    _values: PhantomData<T>,
}

// SAFETY: a value moves from the thread that pushed it to the thread that pops it (`T: Send`),
// and each slot is reached by its one push and then its one pop, as its state orders; blocks
// are made and freed in `A` by any thread that pushes or pops (`A: Send + Sync`).
unsafe impl<T: Send, A: Allocator + Send, const CAP: usize> Send for SegQueue<T, A, CAP> {}
// SAFETY: as above: a `&SegQueue` allocates and frees blocks through `&A` from any thread.
unsafe impl<T: Send, A: Allocator + Sync, const CAP: usize> Sync for SegQueue<T, A, CAP> {}

impl<T, const CAP: usize> SegQueue<T, Global, CAP> {
    /// An empty queue on the global heap.
    ///
    /// Deprecated: the global heap is an ambient allocator (D170 C1). Use
    /// [`SegQueue::new_in`] with the caller's allocator. Kept for this wave's callers alone
    /// (the module doc of the crate lists them).
    pub const fn new() -> SegQueue<T, Global, CAP> {
        SegQueue::new_in(Global)
    }
}

impl<T, A: Allocator, const CAP: usize> SegQueue<T, A, CAP> {
    /// An empty queue whose blocks come from `alloc`. Its first block is allocated by its
    /// first push.
    pub const fn new_in(alloc: A) -> SegQueue<T, A, CAP> {
        SegQueue {
            head: Padded(Position {
                index: AtomicUsize::new(0),
                block: AtomicPtr::new(ptr::null_mut()),
            }),
            tail: Padded(Position {
                index: AtomicUsize::new(0),
                block: AtomicPtr::new(ptr::null_mut()),
            }),
            alloc,
            _values: PhantomData,
        }
    }
}

// Every block is made and freed through `&A` (the queue's own allocator, borrowed), so nothing
// here asks `A: Clone`, and `Drop` frees the same way.
impl<T, A: Allocator, const CAP: usize> SegQueue<T, A, CAP> {
    const LAP: usize = CAP + 1;

    /// Pushes `value`, allocating a block in the queue's allocator when the last is full.
    ///
    /// # Panics
    ///
    /// When the allocator refuses a block (blaze-own's rule for a method without `try_`).
    pub fn push(&self, value: T) {
        let mut tail = self.tail.index.load(Ordering::Acquire);
        let mut block = self.tail.block.load(Ordering::Acquire);
        let mut next_block = None;
        loop {
            let offset = (tail >> SHIFT) % Self::LAP;
            if offset == CAP {
                // Another push is installing the next block.
                SpinWait::until("the next block a SegQueue push is installing", || {
                    self.tail.index.load(Ordering::Acquire) != tail
                });
                tail = self.tail.index.load(Ordering::Acquire);
                block = self.tail.block.load(Ordering::Acquire);
                continue;
            }
            // This push may fill the block: have the next one ready before taking the slot,
            // so the others wait for a store and not an allocation.
            if offset + 1 == CAP && next_block.is_none() {
                next_block = Some(Block::<T, CAP>::new_in(&self.alloc));
            }
            if block.is_null() {
                // The first push: install the first block.
                let (new, _) =
                    Box::into_raw_with_allocator(Block::<T, CAP>::new_in(&self.alloc));
                if self
                    .tail
                    .block
                    .compare_exchange(block, new, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    self.head.block.store(new, Ordering::Release);
                    block = new;
                } else {
                    // SAFETY: `new` is this push's, published nowhere: keep it for the next.
                    next_block = Some(unsafe { Box::from_raw_in(new, &self.alloc) });
                    tail = self.tail.index.load(Ordering::Acquire);
                    block = self.tail.block.load(Ordering::Acquire);
                    continue;
                }
            }
            let new_tail = tail + (1 << SHIFT);
            match self.tail.index.compare_exchange_weak(
                tail,
                new_tail,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if offset + 1 == CAP {
                        // The block is full: install the next and skip the lap's last step.
                        let (next, _) =
                            Box::into_raw_with_allocator(next_block.take().expect("made above"));
                        self.tail.block.store(next, Ordering::Release);
                        self.tail.index.store(new_tail.wrapping_add(1 << SHIFT), Ordering::Release);
                        // SAFETY: `block` is not freed before its last slot is read, which
                        // is not before the value below is written.
                        unsafe { (*block).next.store(next, Ordering::Release) };
                    }
                    // SAFETY: the tail's step at `offset` of `block` is this push's: its slot
                    // is written by no one else and read by no pop before `WRITE` is set.
                    let slot = unsafe { &(*block).slots[offset] };
                    let at = slot.value.get();
                    sync::wrote(at);
                    // SAFETY: as above.
                    unsafe { at.write(MaybeUninit::new(value)) };
                    slot.state.fetch_or(WRITE, Ordering::Release);
                    return;
                }
                Err(now) => {
                    tail = now;
                    block = self.tail.block.load(Ordering::Acquire);
                }
            }
        }
    }

    pub fn pop(&self) -> Option<T> {
        self.pop_with(NoPlant)
    }

    /// [`SegQueue::pop`] with a plant (`NoPlant` outside the model tests).
    pub(crate) fn pop_with(&self, plant: impl Planted<QueuePlant>) -> Option<T> {
        let mut head = self.head.index.load(Ordering::Acquire);
        let mut block = self.head.block.load(Ordering::Acquire);
        loop {
            let offset = (head >> SHIFT) % Self::LAP;
            if offset == CAP {
                // The pop of the block's last slot is moving the head to the next.
                SpinWait::until("the next block a SegQueue pop is moving to", || {
                    self.head.index.load(Ordering::Acquire) != head
                });
                head = self.head.index.load(Ordering::Acquire);
                block = self.head.block.load(Ordering::Acquire);
                continue;
            }
            let mut new_head = head + (1 << SHIFT);
            if new_head & HAS_NEXT == 0 {
                atomic::fence(Ordering::SeqCst);
                let tail = self.tail.index.load(Ordering::Relaxed);
                if head >> SHIFT == tail >> SHIFT {
                    return None;
                }
                // Head and tail in different blocks: the head's is not the last.
                if (head >> SHIFT) / Self::LAP != (tail >> SHIFT) / Self::LAP {
                    new_head |= HAS_NEXT;
                }
            }
            if block.is_null() {
                // The first push has taken the tail but not yet published its block.
                SpinWait::until("the first block of a SegQueue", || {
                    !self.head.block.load(Ordering::Acquire).is_null()
                });
                head = self.head.index.load(Ordering::Acquire);
                block = self.head.block.load(Ordering::Acquire);
                continue;
            }
            match self.head.index.compare_exchange_weak(
                head,
                new_head,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if offset + 1 == CAP {
                        // SAFETY: `block` is freed only below, by this pop or a later reader
                        // of it.
                        let next = unsafe { (*block).wait_next() };
                        let mut next_index = (new_head & !HAS_NEXT).wrapping_add(1 << SHIFT);
                        // SAFETY: `next` is the head's block from here: freed only after its
                        // own last slot is read, which is after this.
                        if !unsafe { (*next).next.load(Ordering::Relaxed) }.is_null() {
                            next_index |= HAS_NEXT;
                        }
                        self.head.block.store(next, Ordering::Release);
                        self.head.index.store(next_index, Ordering::Release);
                    }
                    // SAFETY: the head's step at `offset` of `block` is this pop's: no other
                    // pop takes the slot, and the block is freed only once it is read.
                    let slot = unsafe { &(*block).slots[offset] };
                    slot.wait_write();
                    let at = slot.value.get();
                    sync::read(at);
                    // SAFETY: `WRITE` (acquired above) says the push wrote it.
                    let value = unsafe { at.read().assume_init() };
                    if offset + 1 == CAP {
                        // SAFETY: the last slot read: the block's free starts here.
                        unsafe { Block::destroy(block, 0, &self.alloc, plant) };
                    } else if slot.state.fetch_or(READ, Ordering::AcqRel) & DESTROY != 0 {
                        // The free reached this slot while it was read: go on with it.
                        // SAFETY: every slot below has been read (the free passed them).
                        unsafe { Block::destroy(block, offset + 1, &self.alloc, plant) };
                    }
                    return Some(value);
                }
                Err(now) => {
                    head = now;
                    block = self.head.block.load(Ordering::Acquire);
                }
            }
        }
    }

    /// Whether it held nothing at one moment.
    pub fn is_empty(&self) -> bool {
        let head = self.head.index.load(Ordering::SeqCst);
        let tail = self.tail.index.load(Ordering::SeqCst);
        head >> SHIFT == tail >> SHIFT
    }

    /// The values in it at one moment: a tail read twice the same around a head.
    pub fn len(&self) -> usize {
        let mut spin = SpinWait::new();
        loop {
            let mut tail = self.tail.index.load(Ordering::SeqCst);
            let mut head = self.head.index.load(Ordering::SeqCst);
            if self.tail.index.load(Ordering::SeqCst) == tail {
                tail &= !((1 << SHIFT) - 1);
                head &= !((1 << SHIFT) - 1);
                // A step at a lap's end is the next lap's start.
                if (tail >> SHIFT) % Self::LAP == Self::LAP - 1 {
                    tail = tail.wrapping_add(1 << SHIFT);
                }
                if (head >> SHIFT) % Self::LAP == Self::LAP - 1 {
                    head = head.wrapping_add(1 << SHIFT);
                }
                // Both from the head's block, so the counts fit.
                let lap = (head >> SHIFT) / Self::LAP;
                tail = tail.wrapping_sub((lap * Self::LAP) << SHIFT);
                head = head.wrapping_sub((lap * Self::LAP) << SHIFT);
                tail >>= SHIFT;
                head >>= SHIFT;
                // Each lap's last step holds nothing.
                return tail - head - tail / Self::LAP;
            }
            // The tail moved between the two looks: back off before the next.
            spin.relax();
        }
    }
}

/// Deprecated with [`SegQueue::new`] (the global heap is ambient): kept for this wave's callers.
impl<T, const CAP: usize> Default for SegQueue<T, Global, CAP> {
    fn default() -> SegQueue<T, Global, CAP> {
        SegQueue::new()
    }
}

impl<T, A: Allocator, const CAP: usize> Drop for SegQueue<T, A, CAP> {
    fn drop(&mut self) {
        let mut head = self.head.index.load(Ordering::Relaxed) & !((1 << SHIFT) - 1);
        let tail = self.tail.index.load(Ordering::Relaxed) & !((1 << SHIFT) - 1);
        let mut block = self.head.block.load(Ordering::Relaxed);
        // `&mut self`: no push or pop is under way, so every slot from the head to the tail
        // holds a written value, and every block from the head's on is this queue's alone.
        while head != tail {
            let offset = (head >> SHIFT) % Self::LAP;
            if offset < CAP {
                // SAFETY: as above.
                let at = unsafe { (*block).slots[offset].value.get() };
                sync::read(at);
                // SAFETY: as above.
                unsafe { (*at).assume_init_drop() };
            } else {
                // SAFETY: as above.
                let next = unsafe { (*block).next.load(Ordering::Relaxed) };
                // SAFETY: as above; the block was made through `&self.alloc`.
                drop(unsafe { Box::from_raw_in(block, &self.alloc) });
                block = next;
            }
            head = head.wrapping_add(1 << SHIFT);
        }
        if !block.is_null() {
            // SAFETY: as above.
            drop(unsafe { Box::from_raw_in(block, &self.alloc) });
        }
    }
}

impl<T, A: Allocator, const CAP: usize> fmt::Debug for SegQueue<T, A, CAP> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegQueue").field("len", &self.len()).finish()
    }
}

/// The values of a [`SegQueue`] taken by value, in order: what is left of it when it ends.
pub struct IntoIter<T, A: Allocator, const CAP: usize = 31>(SegQueue<T, A, CAP>);

impl<T, A: Allocator, const CAP: usize> IntoIterator for SegQueue<T, A, CAP> {
    type Item = T;
    type IntoIter = IntoIter<T, A, CAP>;

    fn into_iter(self) -> IntoIter<T, A, CAP> {
        IntoIter(self)
    }
}

impl<T, A: Allocator, const CAP: usize> Iterator for IntoIter<T, A, CAP> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.0.pop()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.0.len();
        (len, Some(len))
    }
}

impl<T, A: Allocator, const CAP: usize> ExactSizeIterator for IntoIter<T, A, CAP> {}

impl<T, A: Allocator, const CAP: usize> fmt::Debug for IntoIter<T, A, CAP> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("IntoIter").field(&self.0).finish()
    }
}
