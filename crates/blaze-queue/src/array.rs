//! The bounded ring: `cap` slots, each with a stamp that says whose turn the slot is.
//!
//! The head and the tail are each an index into the ring and a lap, packed in one word (the
//! index in the low bits, below `one_lap`, the smallest power of two above `cap`). A slot
//! whose stamp equals the tail is free for that push; one whose stamp is the head plus one
//! holds the value that pop takes. A push stores `tail + 1` in its slot's stamp once the value
//! is written; a pop stores `head + one_lap`, the slot's turn on the next lap.

use alloc::{alloc::Global, boxed::Box, vec::Vec};
use core::{cell::UnsafeCell, fmt, mem::MaybeUninit};

use blaze_spin::SpinWait;

use crate::{
    Padded, Plant,
    sync::{
        self,
        atomic::{self, AtomicUsize, Ordering},
    },
};

struct Slot<T> {
    /// The turn: the tail of the push that may write it, or that plus one once written.
    stamp: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

/// A bounded queue of `T` that any number of threads push to and pop from.
pub struct ArrayQueue<T> {
    head: Padded<AtomicUsize>,
    tail: Padded<AtomicUsize>,
    buffer: Box<[Slot<T>], Global>,
    cap: usize,
    one_lap: usize,
}

// SAFETY: a value moves from the thread that pushed it to the thread that pops it (`T: Send`),
// and a slot is reached by one thread at a time, as its stamp hands it over.
unsafe impl<T: Send> Send for ArrayQueue<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for ArrayQueue<T> {}

impl<T> ArrayQueue<T> {
    /// A ring of `cap` slots, allocated now from `Global`.
    ///
    /// # Panics
    ///
    /// When `cap` is 0.
    pub fn new(cap: usize) -> ArrayQueue<T> {
        assert!(cap > 0, "an ArrayQueue of no slots");
        let mut buffer = Vec::with_capacity_in(cap, Global);
        buffer.extend((0..cap).map(|turn| Slot {
            stamp: AtomicUsize::new(turn),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }));
        let one_lap = (cap + 1).next_power_of_two();
        ArrayQueue {
            head: Padded(AtomicUsize::new(0)),
            tail: Padded(AtomicUsize::new(0)),
            buffer: buffer.into_boxed_slice(),
            cap,
            one_lap,
        }
    }

    /// Pushes `value`, or hands it back when the ring is full.
    pub fn push(&self, value: T) -> Result<(), T> {
        self.push_with(value, Plant::None)
    }

    pub(crate) fn push_with(&self, value: T, plant: Plant) -> Result<(), T> {
        let mut tail = self.tail.load(Ordering::Relaxed);
        loop {
            let index = tail & (self.one_lap - 1);
            let lap = tail & !(self.one_lap - 1);
            let next =
                if index + 1 < self.cap { tail + 1 } else { lap.wrapping_add(self.one_lap) };
            let slot = &self.buffer[index];
            let stamp = slot.stamp.load(Ordering::Acquire);
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
                        #[cfg(test)]
                        let publish = match plant {
                            Plant::RelaxedPublish => Ordering::Relaxed,
                            Plant::None | Plant::FreeUnasked => Ordering::Release,
                        };
                        #[cfg(not(test))]
                        let publish = {
                            _ = plant;
                            Ordering::Release
                        };
                        slot.stamp.store(tail + 1, publish);
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
                    slot.stamp.load(Ordering::Acquire) != stamp
                        || self.tail.load(Ordering::Relaxed) != tail
                        || self.head.load(Ordering::Relaxed) != head
                });
                tail = self.tail.load(Ordering::Relaxed);
            } else {
                // Another push or pop is mid-way through the slot.
                SpinWait::until("a slot of an ArrayQueue another thread is writing", || {
                    slot.stamp.load(Ordering::Acquire) != stamp
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
            let stamp = slot.stamp.load(Ordering::Acquire);
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
                        slot.stamp.store(head.wrapping_add(self.one_lap), Ordering::Release);
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
                    slot.stamp.load(Ordering::Acquire) != stamp
                        || self.head.load(Ordering::Relaxed) != head
                        || self.tail.load(Ordering::Relaxed) != tail
                });
                head = self.head.load(Ordering::Relaxed);
            } else {
                SpinWait::until("a slot of an ArrayQueue another thread is writing", || {
                    slot.stamp.load(Ordering::Acquire) != stamp
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

impl<T> Drop for ArrayQueue<T> {
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

impl<T> fmt::Debug for ArrayQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArrayQueue").field("len", &self.len()).field("cap", &self.cap).finish()
    }
}

/// The values of an [`ArrayQueue`] taken by value, in order.
pub struct IntoIter<T>(ArrayQueue<T>);

impl<T> IntoIterator for ArrayQueue<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> IntoIter<T> {
        IntoIter(self)
    }
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.0.pop()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.0.len();
        (len, Some(len))
    }
}

impl<T> ExactSizeIterator for IntoIter<T> {}

impl<T> fmt::Debug for IntoIter<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("IntoIter").field(&self.0).finish()
    }
}
