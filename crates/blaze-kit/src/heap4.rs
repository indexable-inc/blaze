//! [`Heap4<T, A>`]: a 4-ary min-heap in one vector: the scheduler's ready queue, Dijkstra over a
//! call graph, the k-way merge of sorted runs. In place of `BinaryHeap<Reverse<T>>`.
//!
//! MOVED (2026-11-20, rule 30): the one home is now `blaze_own::Heap4` (blaze-own/src/heap4.rs),
//! the same API, its plant on the `Planted` seam (`pop_planted`, `blaze_own::Heap4Plant`) instead
//! of this crate's thread-local switch. This copy stays until its users move, then becomes
//! `pub use blaze_own::Heap4;` and the file goes. Its users: blaze-protocol src/events.rs (12,
//! 159, 170), blaze-graph src/plane.rs (66, 849, 862), blaze-profile src/time/sim.rs (23, 408,
//! 430), and this crate's examples/heaps.rs and src/tests/heaps.rs. Each changes its import
//! from `blaze_kit::Heap4` to `blaze_own::Heap4`, and the three that write `Global` take the
//! caller's allocator instead.
//!
//! The children of slot `i` are `4i + 1 ..= 4i + 4`, so the tree is half as deep as a binary
//! heap's and the four children of a slot are adjacent: for an 8- or 16-byte `T` they share one
//! line. A push sifts up `log4 n` levels with one compare each; a pop sifts the last element
//! down from the root, finding the least of four children per level (three compares, one line).
//! Moves are by a hole, as std's heap does: each level copies one element, never swaps.
//!
//! # Floor
//!
//! `size_of::<T>()` bytes per element, nothing else. A push touches `log4 n` lines, a pop
//! `log4 n` lines plus the root's; a binary heap touches `log2 n` for a pop, twice as many.
//!
//! # Measured (`examples/heaps.rs`, release, one Mac run, 2026-11-15)
//!
//! ns per push and pop against `std::collections::BinaryHeap`: 16.1 against 19.2 at 1e5
//! values, 73.2 against 111.0 at 1e7.

use core::{alloc::Allocator, mem::ManuallyDrop, ptr};

use blaze_own::{AllocFailure, Refused, Vec};

use crate::{
    error::fail,
    plant::{self, Plant},
};

/// Children per slot.
const ARITY: usize = 4;

/// A min-heap of `T`, four children per slot, in `A` (see the module).
pub struct Heap4<T: Ord, A: Allocator> {
    data: Vec<T, A>,
}

/// The element taken out of a slot while it sifts; written back to where it stops on drop, so a
/// panicking compare leaves the heap whole.
struct Hole<'a, T> {
    data: &'a mut [T],
    element: ManuallyDrop<T>,
    at: usize,
}

impl<'a, T> Hole<'a, T> {
    /// Takes slot `at` out.
    ///
    /// # Safety
    ///
    /// `at < data.len()`.
    unsafe fn new(data: &'a mut [T], at: usize) -> Hole<'a, T> {
        // SAFETY: `at` is in bounds (the caller's); the slot is a hole until the drop fills it.
        let element = unsafe { ptr::read(data.get_unchecked(at)) };
        Hole { data, element: ManuallyDrop::new(element), at }
    }

    fn element(&self) -> &T {
        &self.element
    }

    /// The element of slot `index`, which is not the hole.
    ///
    /// # Safety
    ///
    /// `index < data.len()` and `index != at`.
    unsafe fn get(&self, index: usize) -> &T {
        // SAFETY: the caller's.
        unsafe { self.data.get_unchecked(index) }
    }

    /// Moves slot `index`'s element into the hole; the hole is at `index` now.
    ///
    /// # Safety
    ///
    /// `index < data.len()` and `index != at`.
    unsafe fn move_to(&mut self, index: usize) {
        // SAFETY: both in bounds and distinct (the caller's); the hole's slot holds nothing.
        unsafe {
            let base = self.data.as_mut_ptr();
            ptr::copy_nonoverlapping(base.add(index), base.add(self.at), 1);
        }
        self.at = index;
    }
}

impl<T> Drop for Hole<'_, T> {
    fn drop(&mut self) {
        // SAFETY: `at` is in bounds and a hole: the element is written back once.
        unsafe {
            let at = self.at;
            ptr::copy_nonoverlapping(&raw const *self.element, self.data.get_unchecked_mut(at), 1);
        }
    }
}

impl<T: Ord, A: Allocator> Heap4<T, A> {
    pub const fn new_in(alloc: A) -> Heap4<T, A> {
        Heap4 { data: Vec::new_in(alloc) }
    }

    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<Heap4<T, A>, AllocFailure> {
        Ok(Heap4 { data: Vec::try_with_capacity_in(capacity, alloc)? })
    }

    /// [`Heap4::try_with_capacity_in`], panicking with the refusal.
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Heap4<T, A> {
        match Heap4::try_with_capacity_in(capacity, alloc) {
            Ok(heap) => heap,
            Err(failure) => crate::error::fail(failure),
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }

    /// Room for `additional` more pushes without allocating.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.data.try_reserve(additional)
    }

    /// The least element.
    pub fn peek(&self) -> Option<&T> {
        self.data.first()
    }

    /// Adds `value`; refused with it back and nothing changed when there is no room.
    pub fn try_push(&mut self, value: T) -> Result<(), Refused<T>> {
        self.data.try_push(value)?;
        let last = self.data.len() - 1;
        // SAFETY: `last` is the slot just pushed.
        unsafe { self.sift_up(last) };
        Ok(())
    }

    /// [`Heap4::try_push`], panicking with the refusal.
    pub fn push(&mut self, value: T) {
        if let Err(refused) = self.try_push(value) {
            fail(refused.why);
        }
    }

    /// Takes the least element out.
    pub fn pop(&mut self) -> Option<T> {
        let mut last = self.data.pop()?;
        if !self.data.is_empty() {
            core::mem::swap(&mut last, &mut self.data[0]);
            // SAFETY: the heap is not empty.
            unsafe { self.sift_down(0) };
        }
        Some(last)
    }

    /// Pushes `value` and pops the least, in one sift and with no allocation: `value` itself
    /// when it is no greater than the least.
    pub fn push_pop(&mut self, mut value: T) -> T {
        match self.data.first_mut() {
            Some(top) if *top < value => {
                core::mem::swap(top, &mut value);
                // SAFETY: the heap is not empty.
                unsafe { self.sift_down(0) };
                value
            }
            _ => value,
        }
    }

    /// Empties the heap, keeping its room.
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// The elements in heap order (the least first, the rest in no stated order).
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    /// The elements, the room kept, in heap order.
    pub fn drain_unordered(&mut self) -> impl Iterator<Item = T> + '_ {
        core::iter::from_fn(move || self.data.pop())
    }

    /// # Safety
    ///
    /// `at < len`.
    unsafe fn sift_up(&mut self, at: usize) {
        // SAFETY: `at` is in bounds (the caller's).
        let mut hole = unsafe { Hole::new(&mut self.data, at) };
        while hole.at > 0 {
            let parent = (hole.at - 1) / ARITY;
            // SAFETY: `parent < at`, in bounds and not the hole.
            if hole.element() >= unsafe { hole.get(parent) } {
                break;
            }
            // SAFETY: as above.
            unsafe { hole.move_to(parent) };
        }
    }

    /// # Safety
    ///
    /// `at < len`.
    unsafe fn sift_down(&mut self, at: usize) {
        let len = self.data.len();
        let checked = if plant::on(Plant::HeapSkipsFourthChild) { ARITY - 1 } else { ARITY };
        // SAFETY: `at` is in bounds (the caller's).
        let mut hole = unsafe { Hole::new(&mut self.data, at) };
        loop {
            let first = hole.at * ARITY + 1;
            if first >= len {
                break;
            }
            let end = (first + checked).min(len);
            let mut least = first;
            for child in first + 1..end {
                // SAFETY: children are past the hole and below `len`.
                if unsafe { hole.get(child) < hole.get(least) } {
                    least = child;
                }
            }
            // SAFETY: as above.
            if hole.element() <= unsafe { hole.get(least) } {
                break;
            }
            // SAFETY: as above.
            unsafe { hole.move_to(least) };
        }
    }
}

impl<T: Ord + core::fmt::Debug, A: Allocator> core::fmt::Debug for Heap4<T, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.data.iter()).finish()
    }
}
