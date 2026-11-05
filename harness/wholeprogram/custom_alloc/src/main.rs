use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Relaxed);
        BYTES.fetch_add(layout.size(), Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn main() {
    let (calls, bytes) = (ALLOCATIONS.load(Relaxed), BYTES.load(Relaxed));
    let squares: Vec<u64> = (0..1000).map(|it| it * it).collect();
    let text = format!("{}", squares.iter().sum::<u64>());
    let calls = ALLOCATIONS.load(Relaxed) - calls;
    let bytes = BYTES.load(Relaxed) - bytes;
    println!("sum {text}: {calls} allocations of {bytes} bytes through the counting allocator");
}
