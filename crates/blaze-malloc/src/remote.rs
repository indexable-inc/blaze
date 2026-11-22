//! A heap's queue of blocks other threads freed: many producers push one block each with a
//! compare-and-swap of the head, and the one consumer, the heap's owner, takes the whole list with
//! one swap when it next runs short. Taking all at once is what makes it simple: the consumer
//! never reads a node a producer could be pushing behind, so there is no ABA and no epoch, and a
//! block's link word is written by its pusher before the release that publishes it and read by
//! the owner only after the acquire that took it.
//!
//! Why a queue per heap (snmalloc's message queue) and not a list per span (mimalloc's
//! `thread_free`): the owner finds every remote free with one load, whatever span it lands in,
//! and a span that ran full needs no flag for its frees to reach the heap again (mimalloc's
//! "delayed free" state machine). The cost is a second walk over the taken blocks, each found
//! its span by address, which is what a local free costs anyway.
//!
//! The model checker proves the push and the take (`model.rs`), each with a planted bug
//! ([`Plant`]) it must catch.

use core::ptr;

use crate::sync::{
    self,
    atomic::{AtomicPtr, Ordering},
};

/// A freed block as the queue and the free lists link it: its first word is the next one.
#[repr(C)]
pub(crate) struct Block {
    pub(crate) next: *mut Block,
}

/// The pointer a caller handed back to free a block, kept for one use: writing the link into the
/// block's own first word during that free ([`Freed::link`]).
///
/// Why the caller's pointer and not the allocator's (Miri, 2026-11-16, finding 4 in
/// docs/v2/plan/design/malloc.md): a `Box` freed inside a function that took it by value is
/// still protected while `dealloc` runs, and a write through any pointer not derived from it (the
/// allocator's own provenance over the mapping) is a foreign write both aliasing models refuse.
/// A write through the pointer `dealloc` was given is the caller's own. So: metadata outside
/// the block through the allocator's provenance; the block's own bytes, at free time, through
/// this. The later read of the link, when the block is handed out again, goes through the
/// allocator's pointer, and the lists hold only the allocator's pointers.
#[derive(Clone, Copy)]
pub(crate) struct Freed(*mut Block);

impl Freed {
    /// The pointer `block` as the caller gave it, provenance kept.
    #[inline(always)]
    pub(crate) fn of<T>(block: *mut T) -> Freed {
        Freed(block.cast())
    }

    /// Writes `next` as the block's link.
    ///
    /// # Safety
    ///
    /// The block is at least a word, aligned to one, and given up by the caller for this free:
    /// the calling thread is its only writer until the free publishes it.
    #[inline(always)]
    pub(crate) unsafe fn link(self, next: *mut Block) {
        // SAFETY: the caller's.
        unsafe { (*self.0).next = next };
        // The link is the block's first word (`repr(C)`), so the block's address is its.
        sync::wrote(self.0.cast_const());
    }
}

/// A bug a model test plants to show the model catches it; the allocator passes
/// [`Plant::None`], a constant every branch on it folds away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A push publishes with a plain store, not a compare-and-swap: of two frees that race, one
    /// is lost.
    StorePush,
    /// The owner hands out the head it peeked before it takes the list, which still holds it:
    /// a block handed out while it is on the queue, so handed out twice.
    PeekTake,
    /// An adopter starts its heap's queue afresh instead of draining it: a free that reached
    /// the heap after its thread ended is dropped.
    DropAtAdoption,
    /// An abandoned heap is published relaxed: the adopter reads the ended thread's last
    /// writes to it unordered.
    RelaxedAbandon,
}

/// The queue: the head of a list of blocks, newest first.
pub(crate) struct Remote {
    head: AtomicPtr<Block>,
}

impl Remote {
    pub(crate) const fn new() -> Remote {
        Remote { head: AtomicPtr::new(ptr::null_mut()) }
    }

    /// Whether nothing is queued, as a relaxed read: the owner's check before it takes.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.head.load(Ordering::Relaxed).is_null()
    }

    /// Queues `block` for the owner: the queue keeps `block` (the allocator's pointer), and the
    /// link is written through `freed` (the caller's pointer to the same block, [`Freed`]).
    ///
    /// # Safety
    ///
    /// `block` is a block of at least a word that the caller gives up, and `freed` the pointer
    /// it was given back by: nothing else reads or writes it until the owner takes it.
    #[inline]
    pub(crate) unsafe fn push(&self, block: *mut Block, freed: Freed) {
        // SAFETY: the caller's.
        unsafe { self.push_with(block, freed, Plant::None) }
    }

    /// [`Remote::push`] with a planted bug.
    ///
    /// # Safety
    ///
    /// As for [`Remote::push`].
    #[inline]
    pub(crate) unsafe fn push_with(&self, block: *mut Block, freed: Freed, plant: Plant) {
        debug_assert_eq!(block.addr(), freed.0.addr(), "one block, two pointers");
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: the caller gave the block up, so this thread is its only writer until the
            // release below publishes it.
            unsafe { freed.link(head) };
            if plant == Plant::StorePush {
                self.head.store(block, Ordering::Release);
                return;
            }
            // Release: the link written above is the owner's to read once its swap acquires
            // this head.
            match self.head.compare_exchange_weak(head, block, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    /// Takes every queued block and hands each to `each`, newest first. Only the owner calls it.
    ///
    /// # Safety
    ///
    /// The caller is the queue's one consumer (its heap's owner), and `each` takes ownership of
    /// the block it is given.
    #[inline]
    pub(crate) unsafe fn drain(&self, mut each: impl FnMut(*mut Block)) -> usize {
        // SAFETY: the caller's.
        unsafe { self.drain_with(Plant::None, &mut each) }
    }

    /// [`Remote::drain`] with a planted bug.
    ///
    /// # Safety
    ///
    /// As for [`Remote::drain`].
    pub(crate) unsafe fn drain_with(&self, plant: Plant, each: &mut impl FnMut(*mut Block)) -> usize {
        let mut handed = 0;
        if plant == Plant::PeekTake {
            let first = self.head.load(Ordering::Acquire);
            if !first.is_null() {
                each(first);
                handed += 1;
            }
        }
        // Acquire: every link of the list was written before the release that pushed it.
        let mut block = self.head.swap(ptr::null_mut(), Ordering::Acquire);
        while !block.is_null() {
            sync::read(block.cast_const());
            // SAFETY: the swap made this list the owner's alone; each block's link was written
            // before its push released it, and the acquire above ordered it before this read.
            let next = unsafe { (*block).next };
            each(block);
            handed += 1;
            block = next;
        }
        handed
    }

    /// Drops what is queued without handing it out: only a planted bug does this
    /// ([`Plant::DropAtAdoption`]).
    pub(crate) fn forget(&self) {
        self.head.store(ptr::null_mut(), Ordering::Relaxed);
    }
}
