//! A lock-free stack of `u32` indices (Treiber's), whose links live in a side table the owner
//! keeps (`next[index]`). The head packs a 32-bit tag beside the top index and every change
//! bumps the tag, so a pop that read a top which was popped and pushed again in between fails
//! its compare-and-swap instead of linking a stale next (the ABA problem); a tag would have to
//! wrap 2^32 times within one pop for that to slip.
//!
//! Push and pop are one compare-and-swap each when uncontended. The pools and the chunk cache
//! push and pop whole batches through it, so the shared line is written once per 64 objects.

use crate::sync::atomic::{AtomicU32, AtomicU64, Ordering};

mod links;

pub(crate) use links::LinkTable;

/// The empty index: no slot is numbered `u32::MAX`.
pub(crate) const NONE: u32 = u32::MAX;

/// The side table of links.
pub(crate) trait Links {
    /// The link of `index`, which the caller has made exist (committed) before pushing it.
    fn link(&self, index: u32) -> &AtomicU32;
}

const fn pack(tag: u32, index: u32) -> u64 {
    ((tag as u64) << 32) | index as u64
}

const fn index(head: u64) -> u32 {
    head as u32
}

const fn tag(head: u64) -> u32 {
    (head >> 32) as u32
}

pub(crate) struct Stack {
    head: AtomicU64,
}

impl Stack {
    pub(crate) const fn new() -> Stack {
        Stack { head: AtomicU64::new(pack(0, NONE)) }
    }

    /// Pushes `item`, which no one else holds or has on any stack.
    pub(crate) fn push(&self, item: u32, links: &impl Links) {
        debug_assert_ne!(item, NONE);
        let link = links.link(item);
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            link.store(index(head), Ordering::Relaxed);
            // Release: what the pusher wrote of `item` (a batch's contents) is seen by the
            // thread that pops it.
            match self.head.compare_exchange_weak(
                head,
                pack(tag(head).wrapping_add(1), item),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    /// Pops the top item, which the caller then holds alone.
    pub(crate) fn pop(&self, links: &impl Links) -> Option<u32> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let top = index(head);
            if top == NONE {
                return None;
            }
            // The link may be stale if `top` was popped since `head` was read; then the tag
            // moved and the compare-and-swap below fails.
            let next = links.link(top).load(Ordering::Relaxed);
            match self.head.compare_exchange_weak(
                head,
                pack(tag(head).wrapping_add(1), next),
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(top),
                Err(now) => head = now,
            }
        }
    }

    /// Pushes `items` (none on any stack, held by the caller alone) with one compare-and-swap,
    /// `items[0]` on top.
    pub(crate) fn push_all(&self, items: &[u32], links: &impl Links) {
        let Some((&last, _)) = items.split_last() else { return };
        for pair in items.windows(2) {
            debug_assert_ne!(pair[0], NONE);
            links.link(pair[0]).store(pair[1], Ordering::Relaxed);
        }
        debug_assert_ne!(last, NONE);
        let link = links.link(last);
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            link.store(index(head), Ordering::Relaxed);
            // Release: as for `push`, for every item of the chain.
            match self.head.compare_exchange_weak(
                head,
                pack(tag(head).wrapping_add(1), items[0]),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    /// Pops up to `out.len()` items from the top with one compare-and-swap, top first, which
    /// the caller then holds alone. How many it popped.
    pub(crate) fn pop_some(&self, links: &impl Links, out: &mut [u32]) -> usize {
        if out.is_empty() {
            return 0;
        }
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let top = index(head);
            if top == NONE {
                return 0;
            }
            // A link only ever holds `NONE` or an index pushed before, so every one read here
            // names a live link, stale or not; a stale walk leaves the tag moved, and the
            // compare-and-swap below fails. `out.len()` bounds a walk through a stale cycle.
            out[0] = top;
            let mut taken = 1;
            let mut next = links.link(top).load(Ordering::Relaxed);
            while taken < out.len() && next != NONE {
                out[taken] = next;
                taken += 1;
                next = links.link(next).load(Ordering::Relaxed);
            }
            match self.head.compare_exchange_weak(
                head,
                pack(tag(head).wrapping_add(1), next),
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => return taken,
                Err(now) => head = now,
            }
        }
    }

    /// Empties the stack and returns its top, the rest linked from it through `links` down to
    /// [`NONE`]: with `&mut`, no other thread pushes or pops, so no compare-and-swap.
    pub(crate) fn take_all(&mut self) -> u32 {
        let head = self.head.get_mut();
        let top = index(*head);
        *head = pack(tag(*head).wrapping_add(1), NONE);
        top
    }

    pub(crate) fn is_empty(&self) -> bool {
        index(self.head.load(Ordering::Relaxed)) == NONE
    }
}
