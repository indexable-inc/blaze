//! [`Heap4<T, A>`]: a 4-ary min-heap in one vector, the tree's one priority queue: the
//! scheduler's ready queue, Dijkstra over a call graph, the k-way merge of sorted runs, a
//! simulation's event queue. In place of `BinaryHeap<Reverse<T>>`. Moved here from blaze-kit
//! (2026-11-20, rule 30: one concept, one home), so a crate on blaze-own alone has it; blaze-kit's
//! copy is marked moved and goes once its callers are on this one.
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
//! # Measured (blaze-kit's `examples/heaps.rs`, release, one Mac run, 2026-11-15, the same code)
//!
//! ns per push and pop against `std::collections::BinaryHeap`: 16.1 against 19.2 at 1e5
//! values, 73.2 against 111.0 at 1e7.
//!
//! # The plant seam
//!
//! The one planted bug, [`Heap4Plant::SkipsFourthChild`], is taken through a [`Planted`]
//! parameter (D165): [`Heap4::pop_planted`] and [`Heap4::push_pop_planted`]; [`Heap4::pop`] and
//! [`Heap4::push_pop`] pass [`NoPlant`], so the shipped sift compiles the check away.

use core::{alloc::Allocator, mem::ManuallyDrop, ptr};

use crate::{
    AllocFailure, Refused, Vec,
    error::fail,
    planted::{NoPlant, Planted, Plants},
};

/// Children per slot.
const ARITY: usize = 4;

/// The bugs a control can plant in a [`Heap4`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Heap4Plant {
    /// The heap as it is.
    None,
    /// A pop's sift looks at three of a slot's four children, so the least can sit under a
    /// greater parent and come out late.
    SkipsFourthChild,
}

impl Plants for Heap4Plant {}

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
    /// An empty heap in `alloc`; nothing is allocated until it grows.
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
            Err(failure) => fail(failure),
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

    pub fn allocator(&self) -> &A {
        self.data.allocator()
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
        self.pop_planted(NoPlant)
    }

    /// [`Heap4::pop`] with its plant taken through the seam (a control's entry).
    pub fn pop_planted(&mut self, plant: impl Planted<Heap4Plant>) -> Option<T> {
        let mut last = self.data.pop()?;
        if !self.data.is_empty() {
            core::mem::swap(&mut last, &mut self.data[0]);
            // SAFETY: the heap is not empty.
            unsafe { self.sift_down(0, plant) };
        }
        Some(last)
    }

    /// Pushes `value` and pops the least, in one sift and with no allocation: `value` itself
    /// when it is no greater than the least.
    pub fn push_pop(&mut self, value: T) -> T {
        self.push_pop_planted(value, NoPlant)
    }

    /// [`Heap4::push_pop`] with its plant taken through the seam (a control's entry).
    pub fn push_pop_planted(&mut self, mut value: T, plant: impl Planted<Heap4Plant>) -> T {
        match self.data.first_mut() {
            Some(top) if *top < value => {
                core::mem::swap(top, &mut value);
                // SAFETY: the heap is not empty.
                unsafe { self.sift_down(0, plant) };
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
    unsafe fn sift_down(&mut self, at: usize, plant: impl Planted<Heap4Plant>) {
        let len = self.data.len();
        let checked = if plant.has(Heap4Plant::SkipsFourthChild) { ARITY - 1 } else { ARITY };
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
