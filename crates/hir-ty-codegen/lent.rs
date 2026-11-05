//! A list that lends what it holds for as long as it lives, while it keeps growing: how a
//! build keeps the layouts it looked up and hands out `&Layout`s, with no refcount write per
//! lookup (an `Arc` clone per lookup writes a line every thread that holds the layout reads).
//!
//! The one unsafe block of the module is [`Lent::alloc`]'s; its API is safe.

use std::{
    alloc::{Allocator, Global},
    cell::RefCell,
    ops::Deref,
    sync::Arc,
};

/// Values lent for as long as the list lives. Each is behind a pointer of its own, so the
/// list growing moves the pointers, never what they point at. 24 bytes plus one pointer per
/// value; nothing is taken out before the list is dropped. The list's own pointers are
/// `A`'s (a build's frame); what they point at is in allocations of its own.
pub(crate) struct Lent<P, A: Allocator = Global>(RefCell<Vec<P, A>>);

impl<P> Default for Lent<P> {
    fn default() -> Self {
        // alloc: empty, allocates nothing
        Lent(RefCell::new(Vec::new()))
    }
}

impl<P, A: Allocator> Lent<P, A> {
    /// An empty list whose pointers are kept in `alloc`.
    pub(crate) fn new_in(alloc: A) -> Self {
        Lent(RefCell::new(Vec::new_in(alloc)))
    }

    /// An empty list in `room`'s buffer: one a list before gave back ([`Self::into_room`]).
    pub(crate) fn from_room(mut room: Vec<P, A>) -> Self {
        room.clear();
        Lent(RefCell::new(room))
    }

    /// Drops every value kept and gives back the emptied buffer. Taking `self` ends every
    /// loan, so nothing lent is left to point at what this drops.
    pub(crate) fn into_room(self) -> Vec<P, A> {
        let mut room = self.0.into_inner();
        room.clear();
        room
    }
}

/// A pointer whose target stays where it is when the pointer moves.
///
/// # Safety
///
/// Moving a value of the type (into a `Vec`, or by the `Vec` growing) must leave `&*value`
/// pointing at the same live target, valid until the value is dropped.
pub(crate) unsafe trait Stable: Deref {}

// SAFETY: an `Arc`'s target is in its own heap allocation, which moving the `Arc` leaves in
// place and which lives at least as long as this `Arc`.
unsafe impl<T> Stable for Arc<T> {}

// SAFETY: a `Box`'s target is in its own heap allocation, which moving the `Box` leaves in
// place and which lives until the `Box` is dropped.
unsafe impl<T> Stable for Box<T> {}

// SAFETY: a shared reference's target is not where the reference is, and outlives it.
unsafe impl<T> Stable for &T {}

impl<P: Stable, A: Allocator> Lent<P, A> {
    /// Keeps `value` and lends its target for as long as the list lives.
    pub(crate) fn alloc(&self, value: P) -> &P::Target {
        let mut list = self.0.borrow_mut();
        list.push(value);
        let at: *const P::Target = &**list.last().expect("a value was just pushed");
        drop(list);
        // SAFETY: `at` is taken from the pointer in its final place in the list (not before
        // the move into it, which would retag a `Box` and invalidate it under Stacked
        // Borrows). `P` is `Stable`, so later growth of the list moves the pointer and not its
        // target, and nothing is taken out of the list before it is dropped, which `&self`
        // outlives. Taken out early, the target would be freed under a live reference. `A`
        // holds only the list's buffer of pointers; a frame is reset only through
        // `&mut Frame`, which can't be had while the list borrows it.
        unsafe { &*at }
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// What is lent is the value kept, the same as a safe list read by index gives, however
    /// much the list grows after it: seeded values, enough for many reallocations.
    #[test]
    fn a_lent_value_is_the_one_kept_whatever_the_list_does_after() {
        let boxes: Lent<Box<u64>> = Lent::default();
        let arcs: Lent<Arc<u64>> = Lent::default();
        let mut reference = Vec::new();
        let mut lent = Vec::new();
        let mut given = Vec::new();
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for _ in 0..5000 {
            let seed = rng.next_u64();
            reference.push(seed);
            lent.push(boxes.alloc(Box::new(seed)));
            let arc = Arc::new(seed);
            given.push(arc.clone());
            let from_arc = arcs.alloc(arc);
            assert!(std::ptr::eq(from_arc, &**given.last().unwrap()));
        }
        assert!(lent.iter().map(|it| **it).eq(reference.iter().copied()));
        assert!(given.iter().map(|it| **it).eq(reference.iter().copied()));
    }

    /// A list kept in a frame lends as one on the heap does, and its pointers take nothing of
    /// the global allocator once the frame is warm.
    #[test]
    fn a_lent_value_in_a_frame_list_is_the_one_kept() {
        let boxes: Vec<Box<u64>> = (0..512).map(Box::new).collect();
        let lend = |frame: &blaze_mem::Frame| {
            let list: Lent<&u64, &blaze_mem::Frame> = Lent::new_in(frame);
            boxes.iter().map(|it| std::ptr::eq(list.alloc(&**it), &**it)).all(|same| same)
        };
        assert!(blaze_mem::frame::task(blaze_mem::Tag::Codegen, &lend));
        let warm = blaze_alloc_count::assert_at_most(0, "a warm frame list of pointers", || {
            blaze_mem::frame::task(blaze_mem::Tag::Codegen, &lend)
        });
        assert!(warm);
    }

    /// A list made from the room another gave back is empty, lends what it is given, and
    /// grows into that room before it asks for more.
    #[test]
    fn a_list_in_given_back_room_starts_empty() {
        let first: Lent<Box<u64>> = Lent::default();
        for it in 0..64 {
            first.alloc(Box::new(it));
        }
        let room = first.into_room();
        assert!(room.is_empty());
        let capacity = room.capacity();
        let again = Lent::from_room(room);
        let lent = again.alloc(Box::new(7));
        assert_eq!(*lent, 7);
        let room = again.into_room();
        assert!(room.is_empty());
        assert_eq!(room.capacity(), capacity);
    }
}
