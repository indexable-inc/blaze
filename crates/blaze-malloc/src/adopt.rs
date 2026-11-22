//! Which heap a thread gets: a fresh slot, or the heap a thread that ended left behind, adopted
//! whole. An ended thread's heap keeps its segments, with every block still live in them valid,
//! and its queue ([`Remote`]) keeps taking frees from other threads; the adopter takes the queue
//! over with everything in it and frees those blocks into their spans, as the owner would have.
//!
//! The abandoned heaps are a stack of slot indices, its head a 64-bit word of a 32-bit tag over
//! the top index (plus one; 0 is empty): each pop and push moves the tag on, so a pop that read a
//! head and its next, and lost a race in which that head was popped, adopted and abandoned
//! again, fails its compare-and-swap rather than install a stale next (ABA). A push publishes the
//! ended thread's last writes to the heap with release; a pop acquires them.
//!
//! The model checker proves an adoption against a late free (`model.rs`), with planted bugs
//! ([`Plant::DropAtAdoption`], [`Plant::RelaxedAbandon`]) it must catch.

use crate::{
    remote::{Block, Plant, Remote},
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

/// What [`Registry::claim`] gave a thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Claimed {
    /// A slot no thread has used.
    Fresh(u32),
    /// A slot an ended thread left; its queue is drained into the adopter's `each`.
    Adopted(u32),
}

impl Claimed {
    pub(crate) fn index(self) -> u32 {
        match self {
            Claimed::Fresh(at) | Claimed::Adopted(at) => at,
        }
    }
}

/// The slots of `N` heaps: how many were ever handed out, and the stack of abandoned ones.
pub(crate) struct Registry<const N: usize> {
    /// Tag (high 32 bits) and top index plus one (low 32).
    head: AtomicU64,
    /// Slots handed out fresh so far (may pass `N` by the claims that found none).
    fresh: AtomicU32,
    /// Each abandoned slot's next in the stack, plus one.
    next: [AtomicU32; N],
}

/// Packs a tag and an index-plus-one into a head word.
fn pack(tag: u32, top: u32) -> u64 {
    (u64::from(tag) << 32) | u64::from(top)
}

fn unpack(head: u64) -> (u32, u32) {
    ((head >> 32) as u32, head as u32)
}

impl<const N: usize> Registry<N> {
    pub(crate) const fn new() -> Registry<N> {
        Registry {
            head: AtomicU64::new(0),
            fresh: AtomicU32::new(0),
            next: [const { AtomicU32::new(0) }; N],
        }
    }

    /// A heap for a thread that has none: an abandoned one, adopted, its queue handed block by
    /// block to `each` (which frees each into its span), or else a fresh one. `None` when all `N`
    /// slots are taken by threads alive now.
    pub(crate) fn claim(
        &self,
        queue: impl Fn(u32) -> &'static Remote,
        each: impl FnMut(*mut Block),
    ) -> Option<Claimed> {
        self.claim_with(Plant::None, queue, each)
    }

    /// [`Registry::claim`] with a planted bug.
    pub(crate) fn claim_with<'q>(
        &self,
        plant: Plant,
        queue: impl Fn(u32) -> &'q Remote,
        mut each: impl FnMut(*mut Block),
    ) -> Option<Claimed> {
        if let Some(at) = self.pop() {
            let remote = queue(at);
            if plant == Plant::DropAtAdoption {
                remote.forget();
            }
            // SAFETY: the pop made this thread the heap's owner, so its queue's one consumer;
            // `each` takes each block over.
            unsafe { remote.drain(&mut each) };
            return Some(Claimed::Adopted(at));
        }
        let at = self.fresh.fetch_add(1, Ordering::Relaxed);
        if (at as usize) < N { Some(Claimed::Fresh(at)) } else { None }
    }

    /// Leaves slot `at` for another thread to adopt: its owner is ending.
    pub(crate) fn abandon(&self, at: u32) {
        self.abandon_with(at, Plant::None);
    }

    /// [`Registry::abandon`] with a planted bug.
    pub(crate) fn abandon_with(&self, at: u32, plant: Plant) {
        let order = if plant == Plant::RelaxedAbandon { Ordering::Relaxed } else { Ordering::Release };
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            let (tag, top) = unpack(head);
            self.next[at as usize].store(top, Ordering::Relaxed);
            // Release: the ended thread's writes to the heap (its bins, its spans) are the
            // adopter's to read once its pop acquires this head.
            match self.head.compare_exchange_weak(
                head,
                pack(tag.wrapping_add(1), at + 1),
                order,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    fn pop(&self) -> Option<u32> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let (tag, top) = unpack(head);
            let at = top.checked_sub(1)?;
            let next = self.next[at as usize].load(Ordering::Relaxed);
            match self.head.compare_exchange_weak(
                head,
                pack(tag.wrapping_add(1), next),
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(at),
                Err(now) => head = now,
            }
        }
    }
}
