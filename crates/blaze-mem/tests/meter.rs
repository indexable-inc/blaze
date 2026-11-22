//! The meter's allocation counter through the real global allocator (counted.rs), exact on a
//! known sequence; its control is a plant that miscounts by one, which the same check refuses.
//! Two threads allocating at once see only their own (D169).
//!
//! The binary's allocator is [`Planted`]: blaze-mem's `Counted`, and, while [`PLANT`] is on, one
//! allocation more counted to the meter at each allocation of [`PLANTED_SIZE`] bytes: the
//! off-by-one a counting bug would make.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicBool, Ordering};

use blaze_meter::{Count, Meter, Used};

struct Planted;

/// Whether the plant is on: only the control below turns it on, on its own thread, around
/// its own sequence, and only an allocation of [`PLANTED_SIZE`] is miscounted.
static PLANT: AtomicBool = AtomicBool::new(false);
const PLANTED_SIZE: usize = 777;

// SAFETY: every call is `Counted`'s with the caller's arguments; the plant adds to the
// meter's thread-local cells only, which allocates nothing.
unsafe impl GlobalAlloc for Planted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == PLANTED_SIZE && PLANT.load(Ordering::Relaxed) {
            blaze_meter::heap_allocated(1);
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { blaze_mem::counted::Counted.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { blaze_mem::counted::Counted.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as above.
        unsafe { blaze_mem::counted::Counted.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as above.
        unsafe { blaze_mem::counted::Counted.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static HEAP: Planted = Planted;

/// The known sequence: four allocations (777 bytes among them), one grown by `realloc`, all
/// freed; nothing else allocates between the two snapshots (the snapshots allocate nothing).
const SIZES: [usize; 4] = [8, PLANTED_SIZE, 4096, 1 << 20];

fn sequence() -> Used {
    let before = Meter::now();
    let mut held = [core::ptr::null_mut::<u8>(); SIZES.len()];
    for (at, size) in SIZES.iter().enumerate() {
        let layout = Layout::from_size_align(*size, 8).unwrap();
        // SAFETY: a non-zero size; freed below with the same layout.
        held[at] = unsafe { std::alloc::alloc(layout) };
        assert!(!held[at].is_null());
    }
    // SAFETY: `held[0]` is 8 bytes of this layout; grown to 64.
    held[0] = unsafe { std::alloc::realloc(held[0], Layout::from_size_align(8, 8).unwrap(), 64) };
    assert!(!held[0].is_null());
    for (at, size) in SIZES.iter().enumerate() {
        let size = if at == 0 { 64 } else { *size };
        // SAFETY: allocated above with this layout.
        unsafe { std::alloc::dealloc(held[at], Layout::from_size_align(size, 8).unwrap()) };
    }
    Meter::now().since(&before).expect("one thread's pair")
}

/// What the sequence must count: four allocations and the realloc's new block (five), the
/// realloc's old block and the four frees (five frees), the bytes of each.
fn exact(used: &Used) -> Result<(), String> {
    let allocated: u64 = SIZES.iter().map(|it| *it as u64).sum::<u64>() + 64;
    let freed: u64 = 8 + 64 + SIZES[1..].iter().map(|it| *it as u64).sum::<u64>();
    let got = (used.allocations().map_err(|overflow| format!("{overflow}"))?, used.count(Count::HeapBytes), used.count(Count::HeapFrees), used.count(Count::HeapFreedBytes));
    let want = (5, allocated, 5, freed);
    match got == want {
        true => Ok(()),
        false => Err(format!("counted (allocations, bytes, frees, freed bytes) {got:?}, made {want:?}")),
    }
}

#[test]
fn the_allocation_counter_is_exact_and_its_off_by_one_is_seen() {
    let used = sequence();
    exact(&used).unwrap();
    assert_eq!(used.largest_under(), Some(2 << 20), "1 MiB is in [2^20, 2^21)");
    // CONTROL (rule 23): the plant counts one allocation more; the same check refuses it.
    PLANT.store(true, Ordering::Relaxed);
    let planted = sequence();
    PLANT.store(false, Ordering::Relaxed);
    let refused = exact(&planted).expect_err("an off-by-one in the allocation count is seen");
    assert!(refused.contains("(6, "), "{refused}");
}

/// Two threads allocate at once, each a different known sequence of sizes; each thread's
/// stretch holds its own count and bytes exactly.
#[test]
fn two_threads_allocations_do_not_bleed() {
    let start = std::sync::Barrier::new(2);
    let run = |size: usize, times: usize| {
        let start = &start;
        move || {
            let layout = Layout::from_size_align(size, 8).unwrap();
            start.wait();
            let before = Meter::now();
            for _ in 0..times {
                // SAFETY: a non-zero size, freed at once with the same layout.
                unsafe { std::alloc::dealloc(std::alloc::alloc(layout), layout) };
            }
            Meter::now().since(&before).expect("one thread's pair")
        }
    };
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(run(24, 1000));
        let b = scope.spawn(run(4000, 300));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!((a.allocations(), a.count(Count::HeapBytes), a.count(Count::HeapFrees)), (Ok(1000), 24_000, 1000));
    assert_eq!((b.allocations(), b.count(Count::HeapBytes), b.count(Count::HeapFrees)), (Ok(300), 1_200_000, 300));
}
