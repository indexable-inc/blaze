//! An ended thread's heap is adopted with the frees made into it after the end. Its own test
//! binary, with this one test and `Malloc` as no one's global allocator: no other thread ends
//! here, so the heap the next thread claims is the ended one, and the result is the same every
//! run.

use std::{alloc::Allocator, thread};

use core::{alloc::Layout, ptr::NonNull};

use blaze_malloc::Malloc;

/// A block sent between threads, by address.
struct Sent(NonNull<u8>);

// SAFETY: a block is plain memory; the sender gives it up.
unsafe impl Send for Sent {}

#[test]
fn an_ended_threads_heap_is_adopted_with_its_late_frees() {
    let n: u64 = if cfg!(miri) { 200 } else { 5_000 };
    let layout = Layout::from_size_align(48, 8).expect("a layout");
    let adopted = blaze_malloc::source().adopted;
    // A thread allocates blocks, then ends: its heap is abandoned.
    let blocks: Vec<Sent> = thread::spawn(move || {
        (0..n).map(|_| Sent(Malloc.allocate(layout).expect("a block").cast())).collect()
    })
    .join()
    .expect("the allocating thread ends");
    // This thread has no heap: each free goes to the ended heap's queue.
    for Sent(at) in blocks {
        // SAFETY: a live block whose thread ended, given up here.
        unsafe { Malloc.deallocate(at, layout) };
    }
    // The next thread to need a heap adopts that one and takes the queue in.
    let took = thread::spawn(move || {
        let block = Malloc.allocate(layout).expect("a block");
        let stats = blaze_malloc::thread_stats();
        // SAFETY: allocated just above with this layout.
        unsafe { Malloc.deallocate(block.cast(), layout) };
        stats
    })
    .join()
    .expect("the adopting thread ends");
    assert_eq!(blaze_malloc::source().adopted, adopted + 1, "one heap was adopted");
    assert_eq!(took.cross_thread_frees, n, "the adopter took in every late free: {took:?}");
    // What is live in the heap then is the adopter's own one block.
    assert_eq!(took.small_bytes, 48, "and with them every byte of the ended thread's blocks");
}
