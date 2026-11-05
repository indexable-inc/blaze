//! `step` under a `#[global_allocator]` that counts, which the program reads: blaze keeps every
//! allocation (`allocations: 3 * steps`). rustc at opt-level 0 makes the same count; at -O,
//! LLVM may remove some, which `GlobalAlloc`'s contract allows.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

include!("../step.rs");

fn main() {
    let before = ALLOCS.load(Ordering::Relaxed);
    let (n, acc, elapsed) = run();
    let made = ALLOCS.load(Ordering::Relaxed) - before;
    println!("steps {n} result {acc} allocations {made} elapsed_ns {}", elapsed.as_nanos());
}
