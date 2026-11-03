//! The fast paths of `blaze_mem::Kept`: a warm take and give of one block, timed per pair, on
//! one thread and on several at once (each with blocks of its own, all of one class, so they
//! share the class's stack), and a batch taken then given back, as a tree's or a token tree's
//! blocks are (128 KiB a thread, so every thread's together stay under the class's keep and
//! on the warm path). Sizes: a cache line, a small tree, a page; all below the classes that
//! give their pages back as they are given.
//!
//! The last case is the price of giving back: batches of 4 MiB a thread, past the class's
//! keep, so each round trims the class and takes its blocks back cold, page faults and all.
//!
//! What it holds blaze to: a change to the classes costs no more on the warm cases than the
//! tree before it. It prints ns per pair for each case, the best of `ROUNDS`; compare two
//! builds' lines (an A/B run alternating the two binaries).
//!
//! Run: `blaze bench -p blaze-mem --bench kept`.

#![feature(allocator_api)]

use std::{alloc::Allocator, alloc::Layout, hint::black_box, sync::Barrier, time::Instant};

use blaze_mem::{Kept, Tag};

/// Pairs timed per round and case on one thread.
const PAIRS: usize = 1 << 22;
/// Pairs each thread times per round on several threads at once: fewer, as threads sharing a
/// class's stack each pay about a hundred times a lone thread's pair.
const SHARED_PAIRS: usize = PAIRS >> 6;
/// Rounds per case; the best is printed.
const ROUNDS: usize = 7;
/// Bytes a batch takes before giving them back, on the warm path.
const BATCH_BYTES: usize = 128 << 10;
/// Bytes a batch past the class's keep takes.
const PAST_KEEP_BYTES: usize = 4 << 20;

const KEPT: Kept = Kept(Tag::Other);

fn pairs(layout: Layout, count: usize) {
    for _ in 0..count {
        let block = KEPT.allocate(layout).expect("a block");
        black_box(block);
        // SAFETY: the block just taken with `layout`, given back once.
        unsafe { KEPT.deallocate(block.cast(), layout) };
    }
}

fn batch_of(bytes: usize, layout: Layout, count: usize) {
    let blocks = (bytes / layout.size()).max(1);
    let mut held = vec![std::ptr::NonNull::<u8>::dangling(); blocks]; // alloc: the bench's list of held blocks
    for _ in 0..count / blocks {
        for slot in &mut held {
            *slot = KEPT.allocate(layout).expect("a block").cast();
        }
        black_box(&held);
        for &block in held.iter().rev() {
            // SAFETY: each block taken with `layout` just above, given back once.
            unsafe { KEPT.deallocate(block, layout) };
        }
    }
}

fn batches(layout: Layout, count: usize) {
    batch_of(BATCH_BYTES, layout, count);
}

fn batches_past_keep(layout: Layout, count: usize) {
    batch_of(PAST_KEEP_BYTES, layout, count / 8);
}

/// The best of `ROUNDS` of `run(count)` on `threads` threads at once, in ns per pair.
fn timed(threads: usize, run: fn(Layout, usize), layout: Layout) -> f64 {
    let pairs = if threads == 1 { PAIRS } else { SHARED_PAIRS };
    let mut best = f64::MAX;
    for _ in 0..ROUNDS {
        let barrier = Barrier::new(threads);
        let ns = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        run(layout, pairs / 16);
                        barrier.wait();
                        let start = Instant::now();
                        run(layout, pairs);
                        start.elapsed().as_nanos() as f64 / pairs as f64
                    })
                })
                .collect();
            workers.into_iter().map(|worker| worker.join().expect("a worker")).fold(0.0, f64::max)
        });
        best = best.min(ns);
    }
    best
}

fn main() {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    for size in [64, 1000, 4096] {
        let layout = Layout::from_size_align(size, 8).expect("a layout");
        println!("pair  {size:>5} B, 1 thread:  {:.2} ns", timed(1, pairs, layout));
        println!("pair  {size:>5} B, {threads} threads: {:.2} ns", timed(threads, pairs, layout));
        println!("batch {size:>5} B, 1 thread:  {:.2} ns", timed(1, batches, layout));
        println!("batch {size:>5} B, {threads} threads: {:.2} ns", timed(threads, batches, layout));
    }
    let layout = Layout::from_size_align(1000, 8).expect("a layout");
    // An eighth of the pairs each round: the time is per pair of those.
    println!("past keep  1000 B, 1 thread:  {:.2} ns", timed(1, batches_past_keep, layout) * 8.0);
}
