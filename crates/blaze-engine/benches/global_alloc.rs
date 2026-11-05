//! The global allocator's fast paths, as every `Vec`, `Box` and third-party crate in blaze
//! reaches them: an allocation and its free through `std::alloc`, timed per pair, outside any
//! task and inside one (`blaze_mem::frame::task`, whose tag the allocator charges), on one
//! thread and on eight at once, and a batch made on one thread and freed on another, as a memo
//! a query made is dropped by whoever evicts it.
//!
//! blaze-engine's library sets the global allocator (`lib.rs`), so this binary times the same
//! one the daemon runs; it prints which, so a run that timed another says so. The source needs nothing of a particular allocator: build it on two
//! trees and compare their lines (an A/B run alternating the two binaries).
//!
//! Run: `blaze bench -p blaze-engine --bench global_alloc`.

use std::{
    alloc::{Layout, alloc, dealloc},
    hint::black_box,
    sync::Barrier,
    time::Instant,
};

use blaze_mem::Tag;

// Linked for its `#[global_allocator]` (`lib.rs`): a binary that names nothing of a crate leaves
// it out, and would time std's allocator instead.
extern crate blaze_engine;

/// Pairs timed per round and case on one thread.
const PAIRS: usize = 1 << 22;
/// Rounds per case; the best is printed.
const ROUNDS: usize = 7;
/// Threads in the shared cases.
const THREADS: usize = 8;
/// Blocks a batch makes on one thread before another frees them.
const BATCH: usize = 1 << 14;

fn pairs(layout: Layout, count: usize) {
    for _ in 0..count {
        // SAFETY: a nonzero layout; the block is freed once with it.
        unsafe {
            let block = alloc(layout);
            assert!(!block.is_null(), "the allocator failed a {} B block", layout.size());
            black_box(block);
            dealloc(block, layout);
        }
    }
}

/// `f` once on this thread, in a task of `tag` or in none.
fn within(tag: Option<Tag>, f: impl FnOnce()) {
    match tag {
        Some(tag) => blaze_mem::frame::task(tag, |_| f()),
        None => f(),
    }
}

/// The best ns per pair over `ROUNDS`, on one thread.
fn one(tag: Option<Tag>, layout: Layout) -> f64 {
    within(tag, || pairs(layout, PAIRS >> 4));
    (0..ROUNDS)
        .map(|_| {
            let start = Instant::now();
            within(tag, || pairs(layout, PAIRS));
            start.elapsed().as_nanos() as f64 / PAIRS as f64
        })
        .fold(f64::INFINITY, f64::min)
}

/// The best ns per pair over `ROUNDS`, each of `THREADS` threads making its own pairs at once.
fn shared(tag: Option<Tag>, layout: Layout) -> f64 {
    let count = PAIRS >> 3;
    (0..ROUNDS)
        .map(|_| {
            let start = Barrier::new(THREADS + 1);
            let took = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..THREADS) // alloc: the bench's thread handles
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            let began = Instant::now();
                            within(tag, || pairs(layout, count));
                            began.elapsed().as_nanos() as f64
                        })
                    })
                    .collect();
                start.wait();
                workers.into_iter().map(|it| it.join().expect("a bench thread")).fold(0.0, f64::max)
            });
            took / count as f64
        })
        .fold(f64::INFINITY, f64::min)
}

/// The best ns per block over `ROUNDS`: made on this thread in a task of `tag` (or none), then
/// freed on another thread outside any task.
fn across(tag: Option<Tag>, layout: Layout) -> f64 {
    (0..ROUNDS)
        .map(|_| {
            let mut made: Vec<usize> = Vec::with_capacity(BATCH); // alloc: the bench's list of blocks
            let start = Instant::now();
            within(tag, || {
                for _ in 0..BATCH {
                    // SAFETY: a nonzero layout; freed once below.
                    let block = unsafe { alloc(layout) };
                    assert!(!block.is_null(), "the allocator failed a {} B block", layout.size());
                    made.push(block.addr());
                }
            });
            let making = start.elapsed();
            let freed = std::thread::spawn(move || {
                let began = Instant::now();
                for &block in &made {
                    // SAFETY: each block made above with `layout`, freed once.
                    unsafe { dealloc(std::ptr::with_exposed_provenance_mut(block), layout) };
                }
                began.elapsed()
            })
            .join()
            .expect("the freeing thread");
            (making + freed).as_nanos() as f64 / BATCH as f64
        })
        .fold(f64::INFINITY, f64::min)
}

fn main() {
    // A block of a task's, to say whether its tag holds it: the daemon's allocator names it.
    let tagged = blaze_mem::frame::task(Tag::Infer, |_| {
        let before = blaze_mem::tag::snapshot().get(Tag::Infer);
        let held = black_box(vec![0u8; 64 << 20]); // alloc: the bench's probe of the allocator
        let after = blaze_mem::tag::snapshot().get(Tag::Infer);
        black_box(&held);
        drop(held);
        format!("{before:?} -> {after:?}") // alloc: the bench's report
    });
    println!("allocator probe, the infer tag's counters across a 64 MiB block: {tagged}");
    // An argument keeps only the cases whose line contains it, to profile one alone.
    let only = std::env::args().nth(1).unwrap_or_default();
    let case = |line: String, time: &dyn Fn() -> f64| {
        if line.contains(only.as_str()) {
            println!("{line}: {:>7.2} ns", time());
        }
    };
    for size in [64, 1000, 4096] {
        let layout = Layout::from_size_align(size, 8).expect("a valid layout");
        case(format!("pair {size:>5} B, outside any task"), &|| one(None, layout));
        case(format!("pair {size:>5} B, in a task       "), &|| one(Some(Tag::Infer), layout));
        case(format!("pair {size:>5} B, {THREADS} threads in tasks"), &|| {
            shared(Some(Tag::Infer), layout)
        });
        case(format!("made in a task, freed on another thread, {size:>5} B (a block)"), &|| {
            across(Some(Tag::Infer), layout)
        });
    }
}
