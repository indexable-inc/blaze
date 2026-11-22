//! The bounded ring: `cap` slots, each with a stamp that says whose turn the slot is.
//!
//! The head and the tail are each an index into the ring and a lap, packed in one word (the
//! index in the low bits, below `one_lap`, the smallest power of two above `cap`). A slot
//! whose stamp equals the tail is free for that push; one whose stamp is the head plus one
//! holds the value that pop takes. A push stores `tail + 1` in its slot's stamp once the value
//! is written; a pop stores `head + one_lap`, the slot's turn on the next lap.
//!
//! A slot keeps its turn less its own index (wrapping), so slot `i`'s first turn, `i`, is kept
//! as 0 and a new ring is all zero bytes: it is allocated zeroed (`calloc`, fresh pages for a
//! large ring), and no page of it is touched until a value passes through it. A ring of 4,096
//! slots made at start and never used costs the address space, not 4,096 stamp writes and the
//! faults under them. [`Slot::turn`] and [`Slot::set_turn`] are the only reads and writes.

#[allow(blaze::std_use, reason = "the deprecated `ArrayQueue::new` alone (the crate's doc)")]
use alloc::alloc::Global;
use core::{
    alloc::{Allocator, Layout},
    cell::UnsafeCell,
    fmt,
    mem::MaybeUninit,
    ptr::NonNull,
};

use blaze_own::{
    AllocFailure, Box,
    planted::{NoPlant, Planted},
};
use blaze_spin::SpinWait;

use crate::{
    Padded, QueuePlant,
    sync::{
        self,
        atomic::{self, AtomicUsize, Ordering},
    },
};

struct Slot<T> {
    /// The turn (the tail of the push that may write it, or that plus one once written) less
    /// the slot's index, wrapping: zero is slot `i`'s first turn, `i` (the module's doc).
    stamp: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Slot<T> {
    /// The turn of this slot, the one at `index`.
    #[inline(always)]
    fn turn(&self, index: usize, order: Ordering) -> usize {
        self.stamp.load(order).wrapping_add(index)
    }

    /// Sets the turn of this slot, the one at `index`.
    #[inline(always)]
    fn set_turn(&self, index: usize, turn: usize, order: Ordering) {
        self.stamp.store(turn.wrapping_sub(index), order);
    }
}

/// A bounded queue of `T` that any number of threads push to and pop from, its ring in `A`
/// (no default: D170 C1). Shared between threads, `A` is `Send + Sync` (a `SharedAlloc`).
pub struct ArrayQueue<T, A: Allocator> {
    head: Padded<AtomicUsize>,
    tail: Padded<AtomicUsize>,
    buffer: Box<[Slot<T>], A>,
    cap: usize,
    one_lap: usize,
}

// SAFETY: a value moves from the thread that pushed it to the thread that pops it (`T: Send`),
// and a slot is reached by one thread at a time, as its stamp hands it over; the ring is freed
// in `A`, which moves with it (`A: Send`).
unsafe impl<T: Send, A: Allocator + Send> Send for ArrayQueue<T, A> {}
// SAFETY: as above; a `&ArrayQueue` reaches `A` only to drop the ring, under `&mut`.
unsafe impl<T: Send, A: Allocator + Sync> Sync for ArrayQueue<T, A> {}

impl<T> ArrayQueue<T, Global> {
    /// A ring of `cap` slots from the global heap.
    ///
    /// Deprecated: the global heap is an ambient allocator (D170 C1). Use
    /// [`ArrayQueue::new_in`] with the caller's allocator. Kept for this wave's callers alone
    /// (blaze-core cgroup.rs and chan.rs, ix-prof alloc.rs).
    ///
    /// # Panics
    ///
    /// When `cap` is 0, or the heap refuses the ring.
    pub fn new(cap: usize) -> ArrayQueue<T, Global> {
        ArrayQueue::new_in(cap, Global)
    }
}

impl<T, A: Allocator> ArrayQueue<T, A> {
    /// A ring of `cap` slots, allocated now in `alloc`, zeroed and untouched (the module's doc).
    ///
    /// # Panics
    ///
    /// When `cap` is 0, or `alloc` refuses the ring (blaze-own's rule for a method without
    /// `try_`: a panic that fails one job); [`ArrayQueue::try_new_in`] returns the refusal.
    pub fn new_in(cap: usize, alloc: A) -> ArrayQueue<T, A> {
        match ArrayQueue::try_new_in(cap, alloc) {
            Ok(queue) => queue,
            Err(failure) => panic!("blaze_queue: {failure}"),
        }
    }

    /// [`ArrayQueue::new_in`], with a refusal of the ring's memory returned.
    ///
    /// # Panics
    ///
    /// When `cap` is 0.
    pub fn try_new_in(cap: usize, alloc: A) -> Result<ArrayQueue<T, A>, AllocFailure> {
        assert!(cap > 0, "an ArrayQueue of no slots");
        let one_lap = (cap + 1).next_power_of_two();
        Ok(ArrayQueue {
            head: Padded(AtomicUsize::new(0)),
            tail: Padded(AtomicUsize::new(0)),
            buffer: Self::slots(cap, alloc)?,
            cap,
            one_lap,
        })
    }

    /// `cap` slots each at its first turn: zero bytes, from `alloc`'s zeroed allocation.
    fn slots(cap: usize, alloc: A) -> Result<Box<[Slot<T>], A>, AllocFailure> {
        const WHAT: &str = "an ArrayQueue's ring";
        let layout = Layout::array::<Slot<T>>(cap)
            .map_err(|_| AllocFailure::CapacityOverflow { what: WHAT, len: 0, additional: cap })?;
        // A slot holds an `AtomicUsize`, so the layout has a size (`cap > 0`).
        let block = alloc
            .allocate_zeroed(layout)
            .map_err(|_| AllocFailure::Refused { what: WHAT, layout })?;
        let first: NonNull<Slot<T>> = block.cast();
        // Under the model checker the atomics are not plain bytes: each is made in place.
        #[cfg(blaze_check)]
        for at in 0..cap {
            // SAFETY: `at < cap`, inside the block just allocated for `cap` slots, written once.
            unsafe {
                first.add(at).write(Slot {
                    stamp: AtomicUsize::new(0),
                    value: UnsafeCell::new(MaybeUninit::uninit()),
                });
            }
        }
        // SAFETY: the block is `alloc`'s, for `Layout::array::<Slot<T>>(cap)`, which is the
        // layout of the slice of `cap` slots; each slot is valid: outside the model a slot of
        // zero bytes is one (its stamp an `AtomicUsize`, any bits one, 0 being slot `i`'s
        // first turn less `i`; its value a `MaybeUninit`, any bytes), and under it each was
        // written above. Nothing else owns the block.
        Ok(unsafe { Box::from_non_null_in(NonNull::slice_from_raw_parts(first, cap), alloc) })
    }

    /// Pushes `value`, or hands it back when the ring is full.
    pub fn push(&self, value: T) -> Result<(), T> {
        self.push_with(value, NoPlant)
    }

    /// [`ArrayQueue::push`] with a plant (`NoPlant` outside the model tests).
    pub(crate) fn push_with(&self, value: T, plant: impl Planted<QueuePlant>) -> Result<(), T> {
        let mut tail = self.tail.load(Ordering::Relaxed);
        loop {
            let index = tail & (self.one_lap - 1);
            let lap = tail & !(self.one_lap - 1);
            let next =
                if index + 1 < self.cap { tail + 1 } else { lap.wrapping_add(self.one_lap) };
            let slot = &self.buffer[index];
            let stamp = slot.turn(index, Ordering::Acquire);
            if tail == stamp {
                // The slot's turn is this push's: take the tail.
                match self.tail.compare_exchange_weak(
                    tail,
                    next,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let at = slot.value.get();
                        sync::wrote(at);
                        // SAFETY: the tail is this push's, and the slot's stamp said the slot
                        // was free at it: no other thread reaches the value until the stamp
                        // below says it holds one.
                        unsafe { at.write(MaybeUninit::new(value)) };
                        let publish = if plant.has(QueuePlant::RelaxedPublish) {
                            Ordering::Relaxed
                        } else {
                            Ordering::Release
                        };
                        slot.set_turn(index, tail + 1, publish);
                        return Ok(());
                    }
                    Err(now) => tail = now,
                }
            } else if stamp.wrapping_add(self.one_lap) == tail + 1 {
                // The slot holds last lap's value: full, unless a pop has moved the head.
                atomic::fence(Ordering::SeqCst);
                let head = self.head.load(Ordering::Relaxed);
                if head.wrapping_add(self.one_lap) == tail {
                    return Err(value);
                }
                // A pop has taken the head and not yet freed the slot: wait for a change
                // before looking again, as a retry at once would spin with no back-off.
                SpinWait::until("a slot of an ArrayQueue a pop is freeing", || {
                    slot.turn(index, Ordering::Acquire) != stamp
                        || self.tail.load(Ordering::Relaxed) != tail
                        || self.head.load(Ordering::Relaxed) != head
                });
                tail = self.tail.load(Ordering::Relaxed);
            } else {
                // Another push or pop is mid-way through the slot.
                SpinWait::until("a slot of an ArrayQueue another thread is writing", || {
                    slot.turn(index, Ordering::Acquire) != stamp
                        || self.tail.load(Ordering::Relaxed) != tail
                });
                tail = self.tail.load(Ordering::Relaxed);
            }
        }
    }

    /// Pops the oldest value, or `None` when the ring is empty.
    pub fn pop(&self) -> Option<T> {
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            let index = head & (self.one_lap - 1);
            let lap = head & !(self.one_lap - 1);
            let slot = &self.buffer[index];
            let stamp = slot.turn(index, Ordering::Acquire);
            if head + 1 == stamp {
                let next =
                    if index + 1 < self.cap { head + 1 } else { lap.wrapping_add(self.one_lap) };
                match self.head.compare_exchange_weak(
                    head,
                    next,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let at = slot.value.get();
                        sync::read(at);
                        // SAFETY: the head is this pop's, and the stamp (read with acquire
                        // after the push's release) says the slot holds a written value no
                        // other pop takes.
                        let value = unsafe { at.read().assume_init() };
                        slot.set_turn(index, head.wrapping_add(self.one_lap), Ordering::Release);
                        return Some(value);
                    }
                    Err(now) => head = now,
                }
            } else if stamp == head {
                // The slot waits for this lap's push: empty, unless a push has moved the tail.
                atomic::fence(Ordering::SeqCst);
                let tail = self.tail.load(Ordering::Relaxed);
                if tail == head {
                    return None;
                }
                // A push has taken the tail and not yet filled the slot: wait for a change
                // before looking again, as a retry at once would spin with no back-off.
                SpinWait::until("a slot of an ArrayQueue a push is filling", || {
                    slot.turn(index, Ordering::Acquire) != stamp
                        || self.head.load(Ordering::Relaxed) != head
                        || self.tail.load(Ordering::Relaxed) != tail
                });
                head = self.head.load(Ordering::Relaxed);
            } else {
                SpinWait::until("a slot of an ArrayQueue another thread is writing", || {
                    slot.turn(index, Ordering::Acquire) != stamp
                        || self.head.load(Ordering::Relaxed) != head
                });
                head = self.head.load(Ordering::Relaxed);
            }
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn is_empty(&self) -> bool {
        let head = self.head.load(Ordering::SeqCst);
        let tail = self.tail.load(Ordering::SeqCst);
        tail == head
    }

    pub fn is_full(&self) -> bool {
        let tail = self.tail.load(Ordering::SeqCst);
        let head = self.head.load(Ordering::SeqCst);
        head.wrapping_add(self.one_lap) == tail
    }

    /// The values in the ring at one moment: a tail read twice the same around a head.
    pub fn len(&self) -> usize {
        let mut spin = SpinWait::new();
        loop {
            let tail = self.tail.load(Ordering::SeqCst);
            let head = self.head.load(Ordering::SeqCst);
            if self.tail.load(Ordering::SeqCst) == tail {
                let first = head & (self.one_lap - 1);
                let last = tail & (self.one_lap - 1);
                return if first < last {
                    last - first
                } else if first > last {
                    self.cap - first + last
                } else if tail == head {
                    0
                } else {
                    self.cap
                };
            }
            // The tail moved between the two looks: back off before the next.
            spin.relax();
        }
    }
}

impl<T, A: Allocator> Drop for ArrayQueue<T, A> {
    fn drop(&mut self) {
        let head = self.head.load(Ordering::Relaxed);
        let first = head & (self.one_lap - 1);
        for at in 0..self.len() {
            let index = if first + at < self.cap { first + at } else { first + at - self.cap };
            let value = self.buffer[index].value.get();
            sync::read(value);
            // SAFETY: `&mut self`: no push or pop is under way, and the `len` slots from the
            // head each hold a written value no pop took.
            unsafe { (*value).assume_init_drop() };
        }
    }
}

impl<T, A: Allocator> fmt::Debug for ArrayQueue<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArrayQueue").field("len", &self.len()).field("cap", &self.cap).finish()
    }
}

/// The values of an [`ArrayQueue`] taken by value, in order.
pub struct IntoIter<T, A: Allocator>(ArrayQueue<T, A>);

impl<T, A: Allocator> IntoIterator for ArrayQueue<T, A> {
    type Item = T;
    type IntoIter = IntoIter<T, A>;

    fn into_iter(self) -> IntoIter<T, A> {
        IntoIter(self)
    }
}

impl<T, A: Allocator> Iterator for IntoIter<T, A> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.0.pop()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.0.len();
        (len, Some(len))
    }
}

impl<T, A: Allocator> ExactSizeIterator for IntoIter<T, A> {}

impl<T, A: Allocator> fmt::Debug for IntoIter<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("IntoIter").field(&self.0).finish()
    }
}
