//! The kernel's address-space lock under a cold build's memory traffic (2026-11-05: a cold
//! `blaze check --workspace --tests` on 64 cores spent 99.7% of its CPU in the page-fault
//! handler, each `madvise` or `mmap` taking 4 to 5 s).
//!
//! Every thread of the machine does what the query database's workers do on a cold build: takes
//! the next node of one shared column (`Dense::ensure`, then the store of its key, as
//! `node_of` does), carves a row page from one shared heap every few nodes (`Heap::alloc`), and
//! runs a frame task every few nodes, one in `SPAN_EVERY` of which allocates past a chunk (a
//! span). Every page each touches is a first touch.
//!
//! It prints the wall time, the process's user and system CPU and minor faults, and every
//! virtual-memory call blaze made (`blaze_mem::syscalls`), with its total and slowest time.
//! Compare two trees' lines.
//!
//! Run: `blaze run --release -p blaze-mem --example mm`.

#![feature(allocator_api)]

use std::{
    alloc::Layout,
    hint::black_box,
    sync::{
        Barrier,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

use blaze_mem::{Region, Tag, frame, syscalls};

/// Nodes each thread takes.
const NODES_PER_THREAD: usize = 1 << 20;
/// A node row's bytes (`NodeRow`: key and color).
const NODE_BYTES: usize = 16;
/// The column's reservation: the query database's `NODES`.
const NODE_CAPACITY: usize = 1 << 28;
/// A row page carved every this many nodes, of `ROW_PAGE` bytes.
const ROW_EVERY: usize = 4;
const ROW_PAGE: usize = 512;
/// The heap's reservation.
const HEAP_BYTES: usize = 64 << 30;
/// A frame task every this many nodes, allocating `TASK_BYTES` in 256-byte pieces.
const TASK_EVERY: usize = 64;
const TASK_BYTES: usize = 64 << 10;
/// One task in this many also allocates `SPAN_BYTES` at once: past a chunk, so a span.
const SPAN_EVERY: usize = 256;
const SPAN_BYTES: usize = 4 << 20;

fn rusage() -> (f64, f64, u64) {
    // SAFETY: `getrusage` fills the struct it is given; zero is a valid one to start from.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above, a live struct of ours.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    (secs(usage.ru_utime), secs(usage.ru_stime), usage.ru_minflt as u64)
}

fn main() {
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    let nodes = Region::reserve(NODE_CAPACITY * NODE_BYTES, Tag::Query).expect("the node column");
    let heap = Region::reserve(HEAP_BYTES, Tag::Query).expect("the row heap");
    let next_node = AtomicUsize::new(0);
    let heap_top = AtomicUsize::new(0);
    let checksum = AtomicU64::new(0);
    let start = Barrier::new(threads + 1);
    let calls_before = syscalls::snapshot();
    let (user_before, sys_before, faults_before) = rusage();
    let mut began = None;
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let (nodes, heap, next_node, heap_top, checksum, start) =
                (&nodes, &heap, &next_node, &heap_top, &checksum, &start);
            scope.spawn(move || {
                start.wait();
                let mut sum = 0u64;
                for i in 0..NODES_PER_THREAD {
                    let node = next_node.fetch_add(1, Ordering::Relaxed);
                    nodes.ensure((node + 1) * NODE_BYTES).expect("the column grows");
                    let key = nodes.at(node * NODE_BYTES).cast::<AtomicU64>();
                    // SAFETY: the place is committed (just ensured), aligned to 16 and this
                    // thread's alone (its node was taken by the fetch_add).
                    unsafe { key.as_ref() }.store(node as u64 + 1, Ordering::Relaxed);
                    if i % ROW_EVERY == 0 {
                        let at = heap_top.fetch_add(ROW_PAGE, Ordering::Relaxed);
                        heap.ensure(at + ROW_PAGE).expect("the heap grows");
                        let row = heap.at(at).cast::<AtomicU64>();
                        // SAFETY: committed, aligned, this thread's alone, as above.
                        unsafe { row.as_ref() }.store(i as u64, Ordering::Relaxed);
                    }
                    if i % TASK_EVERY == 0 {
                        let span = (i / TASK_EVERY) % SPAN_EVERY == 0;
                        sum += frame::task(Tag::Other, |frame| {
                            let piece = Layout::from_size_align(256, 8).expect("a layout");
                            let mut got = 0u64;
                            for _ in 0..TASK_BYTES / 256 {
                                let ptr = frame.alloc_layout(piece);
                                // SAFETY: 256 fresh bytes of the frame.
                                unsafe { ptr.as_ptr().write(1) };
                                got += 1;
                            }
                            if span {
                                let big = Layout::from_size_align(SPAN_BYTES, 64).expect("a layout");
                                let ptr = frame.alloc_layout(big);
                                // SAFETY: fresh bytes of the frame; one write a page.
                                for page in (0..SPAN_BYTES).step_by(4096) {
                                    unsafe { ptr.as_ptr().add(page).write(1) };
                                }
                                got += 1;
                            }
                            black_box(got)
                        });
                    }
                }
                checksum.fetch_add(sum, Ordering::Relaxed);
            });
        }
        start.wait();
        began = Some(Instant::now());
    });
    // The scope joined every thread: the wall time is from the start to that join.
    let wall = began.expect("the threads started").elapsed().as_secs_f64();
    let (user, sys, faults) = rusage();
    let calls = syscalls::snapshot();
    println!(
        "mm: {threads} threads, {} nodes, {} MiB of row pages, {} frame tasks ({} spans)",
        threads * NODES_PER_THREAD,
        heap_top.load(Ordering::Relaxed) >> 20,
        threads * NODES_PER_THREAD / TASK_EVERY,
        threads * NODES_PER_THREAD / TASK_EVERY / SPAN_EVERY,
    );
    println!(
        "wall {wall:.3} s  user {:.2} s  sys {:.2} s  minor faults {}  (checksum {})",
        user - user_before,
        sys - sys_before,
        faults - faults_before,
        checksum.load(Ordering::Relaxed),
    );
    println!("{:<28} {:>10} {:>12} {:>12}", "call", "count", "total ms", "slowest ms");
    for ((kind, after), (_, before)) in calls.iter().zip(calls_before.iter()) {
        let count = after.calls - before.calls;
        if count == 0 {
            continue;
        }
        println!(
            "{:<28} {:>10} {:>12.1} {:>12.3}",
            kind.name(),
            count,
            (after.ns - before.ns) as f64 / 1e6,
            after.max_ns as f64 / 1e6,
        );
    }
}
