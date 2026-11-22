//! Large blocks give back every mapping: after each is freed (and some moved by a grow, some
//! zeroed), every large mapping still held is one the cache keeps on purpose, and `trim` releases
//! those to zero. Its own binary, so no other test holds a large block meanwhile. Under Miri this
//! also tells a cached mapping (kept, reachable from the cache) from a leaked one: the run's
//! leak check sees nothing left after `trim`.

use std::alloc::Allocator;

use core::alloc::Layout;

use blaze_malloc::Malloc;

#[test]
fn every_large_mapping_is_cached_or_returned() {
    let start = blaze_malloc::source();
    assert_eq!(start.large, 0, "no large block before the test: {start:?}");
    let kib = |n: usize| Layout::from_size_align(n << 10, 8).expect("a layout");
    let mut blocks = Vec::new();
    for (i, n) in [100, 300, 700, 1500, 3000].into_iter().enumerate() {
        let block = if i % 2 == 0 { Malloc.allocate(kib(n)) } else { Malloc.allocate_zeroed(kib(n)) };
        blocks.push((block.expect("a large block").cast::<u8>(), kib(n)));
    }
    // A grow past the reservation moves the block: the old mapping must not be lost.
    let (at, old) = blocks[0];
    let new = kib(5000);
    // SAFETY: a live block with its layout.
    let moved = unsafe { Malloc.grow(at, old, new) }.expect("grown").cast::<u8>();
    blocks[0] = (moved, new);
    let held = blaze_malloc::source();
    assert_eq!(held.large, blocks.len() as u64 + held.large_cached, "one mapping a live block: {held:?}");
    for (at, layout) in blocks {
        // SAFETY: each live block once, with its layout.
        unsafe { Malloc.deallocate(at, layout) };
    }
    let freed = blaze_malloc::source();
    assert_eq!(freed.large, freed.large_cached, "every mapping still held is cached: {freed:?}");
    assert_eq!(blaze_malloc::thread_stats().large_bytes, 0, "every large byte returned");
    blaze_malloc::trim();
    let trimmed = blaze_malloc::source();
    assert_eq!((trimmed.large, trimmed.large_cached), (0, 0), "trim returns them all: {trimmed:?}");
}
