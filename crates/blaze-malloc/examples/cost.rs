//! What blaze-malloc costs against the system allocator (`std::alloc::System`, libmalloc's
//! nano and scalable zones on macOS, glibc's ptmalloc on Linux), row by row. Each row runs both
//! columns in turn, `ROUNDS` times, the first column alternating each round, and keeps each
//! one's best: the machine is shared, and identical code has timed 16% apart on it. (A three-way
//! form of this table, with two out-of-band free-state strategies beside the in-band one, decided
//! between them: docs/v2/plan/design/malloc.md, "Free state: in the block or beside it".)
//!
//! The floor (rule 67): an allocate-then-free pair on the fast path is about twenty instructions
//! (the crate doc lists them), which at the M5's ~4.5 GHz and some four instructions a cycle puts
//! the floor near 1.1 ns a pair. The cold rows' floor is one miss per span header touched; a
//! free list through the blocks adds one miss per block freed out of address order.
//!
//! `cargo run --release -p blaze-malloc --example cost` (through `examplerun.sh`). The
//! fragmentation rows run in child processes of this binary (`cost frag <column>`), since a
//! process's peak resident size is all `getrusage` gives.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    env,
    ffi::c_int,
    hint::black_box,
    process::Command,
    sync::{Arc, Barrier, mpsc},
    thread,
    time::{Duration, Instant},
};

use blaze_malloc::Malloc;
use blaze_rng::SplitMix64;

const ROUNDS: usize = 5;

static SYSTEM: System = System;
static BLAZE: Malloc = Malloc;

/// The columns, in order.
const COLUMNS: [&str; 2] = ["system", "blaze"];

/// An allocator a row runs on.
trait Under: GlobalAlloc + Sync + 'static {}
impl<A: GlobalAlloc + Sync + 'static> Under for A {}

/// Runs each column `ROUNDS` times, the first alternating each round, and prints each one's
/// best per unit and the system's time over blaze's.
fn row(name: &str, units: u64, runs: [&mut dyn FnMut() -> Duration; 2]) {
    let runs = runs;
    let mut best = [Duration::MAX; 2];
    for round in 0..ROUNDS {
        for k in 0..2 {
            let at = (round + k) % 2;
            best[at] = best[at].min(runs[at]());
        }
    }
    let per = |d: Duration| d.as_nanos() as f64 / units as f64;
    println!("{:<42} {:>9.2} {:>9.2} {:>7.2}x", name, per(best[0]), per(best[1]), per(best[0]) / per(best[1]));
}

/// `row` with `$body` run on each column's allocator, bound to `$a`.
macro_rules! row {
    ($name:expr, $units:expr, |$a:ident| $body:expr) => {
        row(
            $name,
            $units,
            [
                &mut || {
                    let $a = &SYSTEM;
                    $body
                },
                &mut || {
                    let $a = &BLAZE;
                    $body
                },
            ],
        )
    };
}

fn timed(f: impl FnOnce()) -> Duration {
    let start = Instant::now();
    f();
    start.elapsed()
}

// ---------------------------------------------------------------------------------------------
// Rows.

/// `n` allocate-then-free pairs of `size` bytes.
fn pairs<A: Under>(a: &'static A, size: usize, n: u64) -> Duration {
    let layout = Layout::from_size_align(size, 8).expect("a layout");
    timed(|| {
        for _ in 0..n {
            // SAFETY: a fresh block of a nonzero layout, freed with it.
            unsafe {
                let p = black_box(a.alloc(layout));
                a.dealloc(p, layout);
            }
        }
    })
}

/// How a batch is freed.
#[derive(Clone, Copy)]
enum Order {
    Same,
    Reverse,
    Shuffled,
}

/// `n` blocks of 64 bytes allocated, then freed in `order` (the shuffle is not timed). With
/// `cold`, the frees alone are timed, after a walk of a buffer larger than every cache.
fn batch<A: Under>(a: &'static A, n: usize, order: Order, cold: Option<&[u64]>) -> Duration {
    let layout = Layout::from_size_align(64, 8).expect("a layout");
    let mut blocks = Vec::with_capacity(n);
    let alloc = timed(|| {
        for _ in 0..n {
            // SAFETY: a nonzero layout.
            let p = unsafe { a.alloc(layout) };
            // SAFETY: a fresh block of 64 bytes: written as a program's would be.
            unsafe { p.cast::<u64>().write(n as u64) };
            blocks.push(p);
        }
    });
    match order {
        Order::Same => {}
        Order::Reverse => blocks.reverse(),
        Order::Shuffled => SplitMix64::new(7).shuffle(&mut blocks),
    }
    let cold = cold.is_some_and(|walk| {
        let mut sum = 0u64;
        for &w in walk {
            sum = sum.wrapping_add(w);
        }
        black_box(sum);
        true
    });
    let free = timed(|| {
        for &p in &blocks {
            // SAFETY: each block once, with its layout.
            unsafe { a.dealloc(p, layout) };
        }
    });
    if cold { free } else { alloc + free }
}

/// A pointer sent to another thread.
struct Sent(Vec<usize>);

/// One thread allocates `n` blocks of 64 bytes and sends them in chunks; another frees them.
fn cross<A: Under>(a: &'static A, n: usize) -> Duration {
    let layout = Layout::from_size_align(64, 8).expect("a layout");
    let (send, recv) = mpsc::sync_channel::<Sent>(64);
    let start = Arc::new(Barrier::new(2));
    let freer = {
        let start = Arc::clone(&start);
        thread::spawn(move || {
            start.wait();
            for Sent(chunk) in recv {
                for p in chunk {
                    // SAFETY: a block the other thread allocated and gave up.
                    unsafe { a.dealloc(p as *mut u8, layout) };
                }
            }
        })
    };
    start.wait();
    timed(|| {
        let mut chunk = Vec::with_capacity(1024);
        for _ in 0..n {
            // SAFETY: a nonzero layout.
            chunk.push(unsafe { a.alloc(layout) }.addr());
            if chunk.len() == 1024 {
                send.send(Sent(std::mem::replace(&mut chunk, Vec::with_capacity(1024)))).expect("the freer listens");
            }
        }
        send.send(Sent(chunk)).expect("the freer listens");
        drop(send);
        freer.join().expect("the freer ends");
    })
}

/// `threads` threads each replace random slots of their own 4096-slot table `ops` times, sizes
/// 16 to 1024 bytes: allocation under contention for nothing but the page source.
fn churn<A: Under>(a: &'static A, threads: usize, ops: usize) -> Duration {
    let start = Arc::new(Barrier::new(threads + 1));
    let handles: Vec<_> = (0..threads)
        .map(|me| {
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let mut rng = SplitMix64::new(me as u64 + 11);
                let mut slots: Vec<(usize, Layout)> = Vec::with_capacity(4096);
                start.wait();
                for i in 0..ops {
                    let layout = Layout::from_size_align(rng.range(16..1025) as usize, 8).expect("a layout");
                    // SAFETY: a nonzero layout.
                    let p = unsafe { a.alloc(layout) };
                    // SAFETY: a fresh block of at least 16 bytes.
                    unsafe { p.write(i as u8) };
                    if slots.len() < 4096 {
                        slots.push((p.addr(), layout));
                    } else {
                        let at = rng.index(4096);
                        let (old, old_layout) = std::mem::replace(&mut slots[at], (p.addr(), layout));
                        // SAFETY: a block of this thread's, with its layout.
                        unsafe { a.dealloc(old as *mut u8, old_layout) };
                    }
                }
                for (p, layout) in slots {
                    // SAFETY: as above.
                    unsafe { a.dealloc(p as *mut u8, layout) };
                }
            })
        })
        .collect();
    start.wait();
    timed(|| {
        for h in handles {
            h.join().expect("a churning thread ends");
        }
    })
}

/// A compiler's shape, single-threaded: per "item", many small nodes (16 to 128 bytes, most
/// 32 to 64), a few vectors grown by doubling (realloc), a string buffer or two of a few KiB,
/// and now and then a 64 KiB table; an item's nodes freed in reverse when it ends, a tenth kept
/// for the "crate" and freed at the end. Returns the operations made.
fn compiler<A: Under>(a: &'static A, items: usize) -> (Duration, u64) {
    let mut ops = 0u64;
    let took = timed(|| {
        let mut rng = SplitMix64::new(3);
        let mut kept: Vec<(usize, Layout)> = Vec::new();
        let mut nodes: Vec<(usize, Layout)> = Vec::with_capacity(512);
        for _ in 0..items {
            for _ in 0..rng.range(50..400) {
                let size = match rng.below(10) {
                    0 | 1 => 16,
                    2..=6 => 32 + 8 * rng.below(5) as usize,
                    7 | 8 => 72 + 8 * rng.below(8) as usize,
                    _ => 128,
                };
                let layout = Layout::from_size_align(size, 8).expect("a layout");
                // SAFETY: a nonzero layout.
                let p = unsafe { a.alloc(layout) };
                // SAFETY: a fresh block of at least 16 bytes: a node is written when made.
                unsafe { p.cast::<u64>().write(size as u64) };
                ops += 1;
                if rng.one_in(10) { kept.push((p.addr(), layout)) } else { nodes.push((p.addr(), layout)) }
            }
            for _ in 0..rng.range(2..8) {
                let mut layout = Layout::from_size_align(32, 8).expect("a layout");
                // SAFETY: a nonzero layout.
                let mut p = unsafe { a.alloc(layout) };
                ops += 1;
                for _ in 0..rng.range(1..8) {
                    let bigger = layout.size() * 2;
                    // SAFETY: a live block with its layout, grown.
                    p = unsafe { a.realloc(p, layout, bigger) };
                    layout = Layout::from_size_align(bigger, 8).expect("a layout");
                    ops += 1;
                }
                nodes.push((p.addr(), layout));
            }
            if rng.one_in(4) {
                let layout = Layout::from_size_align(rng.range(1024..8192) as usize, 8).expect("a layout");
                // SAFETY: a nonzero layout.
                nodes.push((unsafe { a.alloc(layout) }.addr(), layout));
                ops += 1;
            }
            if rng.one_in(50) {
                let layout = Layout::from_size_align(64 << 10, 8).expect("a layout");
                // SAFETY: a nonzero layout.
                nodes.push((unsafe { a.alloc_zeroed(layout) }.addr(), layout));
                ops += 1;
            }
            while let Some((p, layout)) = nodes.pop() {
                // SAFETY: each block once, with its layout.
                unsafe { a.dealloc(p as *mut u8, layout) };
                ops += 1;
            }
        }
        for (p, layout) in kept {
            // SAFETY: as above.
            unsafe { a.dealloc(p as *mut u8, layout) };
            ops += 1;
        }
    });
    (took, ops)
}

// ---------------------------------------------------------------------------------------------
// Fragmentation, in a child process.

#[repr(C)]
struct Rusage {
    times: [i64; 4],
    maxrss: i64,
    rest: [i64; 13],
}

unsafe extern "C" {
    fn getrusage(who: c_int, usage: *mut Rusage) -> c_int;
}

/// The process's peak resident bytes.
fn peak_rss() -> u64 {
    let mut usage = Rusage { times: [0; 4], maxrss: 0, rest: [0; 13] };
    // SAFETY: `usage` is a `struct rusage` (two timevals and fourteen longs) to fill.
    let got = unsafe { getrusage(0, &mut usage) };
    assert_eq!(got, 0, "getrusage failed");
    let maxrss = usage.maxrss as u64;
    if cfg!(target_os = "macos") { maxrss } else { maxrss * 1024 }
}

/// The child: phases of different size mixes, each filling a live set to `LIVE` bytes, then
/// freeing three blocks in four at random, so each phase must reuse the holes the last left.
/// Prints the peak resident bytes above the start, and the peak bytes live.
fn frag<A: Under>(a: &'static A) {
    const LIVE: u64 = 256 << 20;
    let base = peak_rss();
    let mut rng = SplitMix64::new(5);
    let mut live: Vec<(usize, Layout)> = Vec::with_capacity(8 << 20);
    let mut bytes = 0u64;
    let mut peak = 0u64;
    let mixes: [(u64, u64); 6] = [(16, 128), (128, 1024), (16, 64), (1024, 8192), (8192, 65536), (24, 512)];
    for (low, high) in mixes {
        while bytes < LIVE {
            let layout = Layout::from_size_align(rng.range(low..high + 1) as usize, 8).expect("a layout");
            // SAFETY: a nonzero layout.
            let p = unsafe { a.alloc(layout) };
            // SAFETY: a fresh block: touch its pages as a program would.
            unsafe { p.write_bytes(1, layout.size()) };
            live.push((p.addr(), layout));
            bytes += layout.size() as u64;
        }
        peak = peak.max(bytes);
        let mut i = 0;
        while i < live.len() {
            if rng.below(4) != 0 {
                let (p, layout) = live.swap_remove(i);
                // SAFETY: each block once, with its layout.
                unsafe { a.dealloc(p as *mut u8, layout) };
                bytes -= layout.size() as u64;
            } else {
                i += 1;
            }
        }
    }
    println!("{} {}", peak_rss() - base, peak);
}

fn frag_row() {
    let exe = env::current_exe().expect("this binary's path");
    let run = |which: &str| -> (u64, u64) {
        let out = Command::new(&exe).args(["frag", which]).output().expect("the child runs");
        assert!(out.status.success(), "the {which} child failed: {}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).expect("text");
        let mut words = text.split_whitespace().map(|w| w.parse::<u64>().expect("a number"));
        (words.next().expect("rss"), words.next().expect("live"))
    };
    let mut best = [u64::MAX; 2];
    let mut live = 0;
    for round in 0..3 {
        for k in 0..2 {
            let at = (round + k) % 2;
            let (rss, l) = run(COLUMNS[at]);
            live = l;
            best[at] = best[at].min(rss);
        }
    }
    let mib = |x: u64| x as f64 / (1 << 20) as f64;
    println!(
        "{:<42} {:>9.0} {:>9.0} {:>7.2}x   (MiB peak resident; peak live {:.0} MiB; {})",
        "fragmentation: peak rss",
        mib(best[0]),
        mib(best[1]),
        best[0] as f64 / best[1] as f64,
        mib(live),
        best.iter().zip(COLUMNS).map(|(&b, c)| format!("{c} {:.2}x live", b as f64 / live as f64)).collect::<Vec<_>>().join(", "),
    );
}

// ---------------------------------------------------------------------------------------------
// The thread's word.

/// Nanoseconds per read of `read`, best of `ROUNDS`.
fn per_read(read: impl Fn() -> *const u8) -> f64 {
    const N: u64 = 20_000_000;
    let mut best = Duration::MAX;
    for _ in 0..ROUNDS {
        best = best.min(timed(|| {
            for _ in 0..N {
                black_box(read());
            }
        }));
    }
    best.as_nanos() as f64 / N as f64
}

fn tls_rows() {
    // The key exists and this thread's word is set once something was allocated.
    // SAFETY: a nonzero layout, freed at once.
    unsafe { BLAZE.dealloc(BLAZE.alloc(Layout::new::<u64>()), Layout::new::<u64>()) };
    assert!(blaze_malloc::probe::ready(), "the thread has a heap");
    println!("{:<42} {:>9.2}", "tls: #[thread_local] read (ns)", per_read(blaze_malloc::probe::thread_local));
    println!(
        "{:<42} {:>9.2}",
        "tls: pthread_getspecific (ns)",
        per_read(|| blaze_malloc::probe::pthread().cast_const().cast())
    );
    println!("{:<42} {:>9.2}", "tls: chosen read (ns)", per_read(blaze_malloc::probe::chosen));
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("frag") {
        match args.get(2).map(String::as_str) {
            Some("system") => frag(&SYSTEM),
            Some("blaze") => frag(&BLAZE),
            other => panic!("frag takes one of {COLUMNS:?}, not {other:?}"),
        }
        return;
    }
    println!("{:<42} {:>9} {:>9} {:>8}", "row (ns per unit, best of 5)", COLUMNS[0], COLUMNS[1], "ratio");
    for size in [16, 64, 256, 1024, 4096] {
        let n = 5_000_000;
        row!(&format!("pair {size} B"), n, |a| pairs(a, size, n));
    }
    const BATCH: usize = 1 << 20;
    for (name, order) in [("same", Order::Same), ("reverse", Order::Reverse), ("shuffled", Order::Shuffled)] {
        row!(&format!("batch 1M x 64 B, free {name}"), BATCH as u64, |a| batch(a, BATCH, order, None));
    }
    // 256 MiB: past the M5's caches (and the system cache) several times over.
    let walk: Vec<u64> = (0..32u64 << 20).collect();
    for (name, order) in [("same", Order::Same), ("shuffled", Order::Shuffled)] {
        row!(&format!("cold free 1M x 64 B, {name} (free only)"), BATCH as u64, |a| batch(
            a,
            BATCH,
            order,
            Some(&walk)
        ));
    }
    let n = 2_000_000;
    row!("cross-thread 64 B (per block)", n as u64, |a| cross(a, n));
    for threads in [1, 6, 12, 18] {
        let ops = 1_000_000;
        row!(&format!("churn {threads} threads (per op, wall)"), (threads * ops) as u64, |a| churn(a, threads, ops));
    }
    let items = 20_000;
    // The trace is the same on every column (one seed): count its operations once.
    let (_, total) = compiler(&BLAZE, items);
    row!("compiler-shaped trace (per op)", total, |a| compiler(a, items).0);
    frag_row();
    tls_rows();
    let stats = blaze_malloc::stats();
    let source = blaze_malloc::source();
    println!("blaze after the run: {stats:?}");
    println!("blaze's page source: {source:?}");
}
