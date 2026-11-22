//! The one write into a freed block through the allocator's own provenance, forced: a block
//! freed on another thread reaches its owner through the owner's queue, and the owner, draining
//! it, re-links it into its span (`heap::free_owned`) while the freeing thread is still inside
//! the function that took the block's `Box` by value, so the `Box` is still protected.
//!
//! The order is forced, not left to the scheduler: the freer drops the box inside `f`, then
//! parks there until the owner thread has ended (its heap's thread-end destructor drains the
//! queue and re-links every block), and only then returns. Under Miri this says what the
//! aliasing models make of that write; natively it checks the free arrived (`stats`).
//!
//! Expected to fail under Miri, so ignored there (`--ignored` shows the report). Both aliasing
//! models refuse it (coordinator's run, 2026-11-16): Stacked Borrows "would remove [Unique ...]
//! which is weakly protected", Tree Borrows "read access through ... (root of the allocation)
//! ... is forbidden". The cause is the in-block free list itself: the owner must read and write
//! the link word of a block it took from the queue, through its own provenance, since no pointer
//! the freer handed over is at hand there. Deferring the re-link removes the write, not the read.
//! Natively nothing is wrong: the freer gave the block up; the models' protector outlives the
//! drop because `f` has not returned. The out-of-band form (free bits in the span header), which
//! never touches a freed block, was built, measured slower on blaze's workloads and deleted
//! (docs/v2/plan/design/malloc.md, "Free state: in the block or beside it").

use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

#[global_allocator]
static ALLOC: blaze_malloc::Malloc = blaze_malloc::Malloc;

/// Set by the freer once `drop(b)` has run inside `f`.
static DROPPED: AtomicBool = AtomicBool::new(false);
/// Set by the main thread once the owner has ended (so drained and re-linked).
static OWNER_ENDED: AtomicBool = AtomicBool::new(false);

/// Spins until `flag` is set, at most a minute: a hang fails, naming what it waited for.
fn wait_for(what: &str, flag: &AtomicBool) {
    let until = Instant::now() + Duration::from_secs(60);
    while !flag.load(Ordering::Acquire) {
        assert!(Instant::now() < until, "waited a minute for {what}");
        thread::yield_now();
    }
}

/// Takes the box by value, so it is protected until this returns; frees it, then stays until
/// the owner has re-linked it.
#[inline(never)]
fn f(b: Box<u64>) -> u64 {
    let seen = *b;
    drop(b);
    DROPPED.store(true, Ordering::Release);
    wait_for("the owner thread to end", &OWNER_ENDED);
    seen
}

#[test]
#[cfg_attr(miri, ignore = "refused by both aliasing models: an in-block list reads and writes a drained block (see the file's note)")]
fn a_remote_free_is_relinked_while_its_box_is_still_protected() {
    let before = blaze_malloc::stats().cross_thread_frees;
    let owner = thread::spawn(|| {
        // The box is the owner's block: its span's heap is this thread's.
        let b = Box::new(11u64);
        let freer = thread::spawn(move || f(b));
        wait_for("the freer to drop the box inside f", &DROPPED);
        // Returning ends this thread: its heap's destructor drains the queue, re-linking the
        // box's block while `f` still holds the box's protection.
        freer
    });
    let freer = owner.join().expect("the owner ends");
    OWNER_ENDED.store(true, Ordering::Release);
    assert_eq!(freer.join().expect("the freer ends"), 11);
    let after = blaze_malloc::stats().cross_thread_frees;
    assert!(after > before, "the box's free reached its owner through the queue: {before} -> {after}");
}
