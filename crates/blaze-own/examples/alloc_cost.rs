//! What the allocator handle costs: nanoseconds per `push` into a growing vector and per small
//! allocation (a box made and dropped), through a bump allocator named directly (as a hot loop
//! names `&Frame`), through an `Alloc` handle over the same bump allocator, and through the
//! global allocator, directly and through a handle.
//!
//! `cargo run --release -p blaze-own --example alloc_cost`
//!
//! Then the fast paths of `blaze_own::spec`, each against the generic way (values behind a
//! wrapper that is not `TrivialClone`, an iterator behind one that is not `TrustedLen`).
//!
//! The handles pass through `black_box`, so the compiler cannot see which table they point at
//! and every call is the indirect call a function taking `Alloc<'_>` makes.

#![feature(allocator_ext)]

use std::{
    alloc::{AllocError, Allocator, Global, Layout},
    cell::Cell,
    hint::black_box,
    ptr::NonNull,
    time::Instant,
};

use blaze_own::{Alloc, AllocFailure, Box, CloneIn, CollectIn, Erased, Soa, Vec, VecDeque};

/// A bump allocator over one buffer, like a task's frame: the newest block is freed, grown and
/// shrunk in place; older frees wait for `reset`.
struct Bump {
    base: NonNull<u8>,
    cap: usize,
    top: Cell<usize>,
}

const BUMP_ALIGN: usize = 4096;

impl Bump {
    fn new(cap: usize) -> Bump {
        let Ok(layout) = Layout::from_size_align(cap, BUMP_ALIGN) else { panic!("a bump buffer of {cap} bytes") };
        // SAFETY: the layout is not sized 0 (the callers ask for megabytes).
        let raw = unsafe { std::alloc::alloc(layout) };
        let Some(base) = NonNull::new(raw) else { panic!("the system refused {cap} bytes for the bump buffer") };
        Bump { base, cap, top: Cell::new(0) }
    }

    fn reset(&self) {
        self.top.set(0);
    }

    #[inline(always)]
    fn offset(&self, ptr: NonNull<u8>) -> usize {
        ptr.addr().get() - self.base.addr().get()
    }

    #[inline(always)]
    fn block(&self, start: usize, size: usize) -> NonNull<[u8]> {
        // SAFETY: `start <= cap`, inside the buffer or one past it.
        NonNull::slice_from_raw_parts(unsafe { self.base.add(start) }, size)
    }
}

impl Drop for Bump {
    fn drop(&mut self) {
        let Ok(layout) = Layout::from_size_align(self.cap, BUMP_ALIGN) else { return };
        // SAFETY: allocated in `new` with this layout.
        unsafe { std::alloc::dealloc(self.base.as_ptr(), layout) };
    }
}

// SAFETY: blocks are disjoint ranges of the buffer below `top`, inside `cap`; a range is handed
// out again only after it was freed (the newest, in place) or after `reset`, which the example
// calls only with nothing live. Alignment up to the buffer's own is kept by rounding the offset.
unsafe impl Allocator for &Bump {
    #[inline(always)]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.align() > BUMP_ALIGN {
            return Err(AllocError);
        }
        let start = self.top.get().next_multiple_of(layout.align());
        let end = start.checked_add(layout.size()).ok_or(AllocError)?;
        if end > self.cap {
            return Err(AllocError);
        }
        self.top.set(end);
        Ok(self.block(start, layout.size()))
    }

    #[inline(always)]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let start = self.offset(ptr);
        if start + layout.size() == self.top.get() {
            self.top.set(start);
        }
    }

    #[inline(always)]
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let start = self.offset(ptr);
        if start + old.size() == self.top.get() && start % new.align() == 0 && start + new.size() <= self.cap {
            self.top.set(start + new.size());
            return Ok(self.block(start, new.size()));
        }
        let block = self.allocate(new)?;
        // SAFETY: the old block has `old.size()` bytes, the new one at least as many, disjoint.
        unsafe { ptr.copy_to_nonoverlapping(block.cast(), old.size()) };
        Ok(block)
    }

    #[inline(always)]
    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let start = self.offset(ptr);
        if start % new.align() != 0 {
            return Err(AllocError);
        }
        if start + old.size() == self.top.get() {
            self.top.set(start + new.size());
        }
        Ok(self.block(start, new.size()))
    }
}

/// Pushes per measurement, whatever the vector's final length.
const PUSHES: usize = 1 << 25;
/// Small allocations per measurement.
const BOXES: usize = 1 << 24;

/// Nanoseconds per push: vectors of `len` built from empty, `PUSHES / len` times.
#[inline(never)]
fn per_push<A: Allocator + Copy>(alloc: A, len: usize, reset: &dyn Fn()) -> f64 {
    let rounds = PUSHES / len;
    let start = Instant::now();
    for _ in 0..rounds {
        let mut v = Vec::new_in(alloc);
        for i in 0..len {
            v.push(black_box(i as u64));
        }
        black_box(v.as_slice());
        drop(v);
        reset();
    }
    start.elapsed().as_nanos() as f64 / (rounds * len) as f64
}

/// Nanoseconds per box of `BYTES` made and dropped.
#[inline(never)]
fn per_box<A: Allocator + Copy, const BYTES: usize>(alloc: A) -> f64 {
    let start = Instant::now();
    for _ in 0..BOXES {
        let b = Box::new_in(black_box([7u8; BYTES]), alloc);
        black_box(&*b);
    }
    start.elapsed().as_nanos() as f64 / BOXES as f64
}

/// A value as it is, but not `TrivialClone`, so the collections take it the generic way.
#[derive(Clone, PartialEq)]
struct Plain<T>(T);

impl<T: Clone, B: Allocator> CloneIn<B> for Plain<T> {
    type In = Plain<T>;

    fn try_clone_in(&self, _alloc: B) -> Result<Plain<T>, AllocFailure> {
        Ok(self.clone())
    }
}

/// An iterator as it is, but not `TrustedLen`.
struct Hidden<I>(I);

impl<I: Iterator> Iterator for Hidden<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        self.0.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// Values per call in the fast-path table.
const VALUES: usize = 4096;
/// Calls per measurement in the fast-path table.
const CALLS: usize = 4096;

/// Nanoseconds per value: `CALLS` calls of `call`, each over `VALUES` values.
#[inline(never)]
fn per_value(call: impl Fn()) -> f64 {
    let start = Instant::now();
    for _ in 0..CALLS {
        call();
    }
    start.elapsed().as_nanos() as f64 / (CALLS * VALUES) as f64
}

/// The fast paths of `blaze_own::spec` against the generic way, which `Plain` and `Hidden` force.
fn fast_paths() {
    let words: std::vec::Vec<u32> = (0..VALUES as u32).collect();
    let plain_words: std::vec::Vec<Plain<u32>> = words.iter().copied().map(Plain).collect();
    let bytes = std::vec![7u8; VALUES];
    let plain_bytes: std::vec::Vec<Plain<u8>> = bytes.iter().copied().map(Plain).collect();
    let fast_vec = Vec::from_slice_in(&words, Global);
    let plain_vec = Vec::from_slice_in(&plain_words, Global);
    let byte_vec = Vec::from_slice_in(&bytes, Global);
    let plain_byte_vec = Vec::from_slice_in(&plain_bytes, Global);
    let mut fast_rows: Soa<(u8, u32), Global> = Soa::new_in(Global);
    let mut plain_rows: Soa<(Plain<u8>, u32), Global> = Soa::new_in(Global);
    for k in 0..VALUES {
        fast_rows.push((k as u8, k as u32));
        plain_rows.push((Plain(k as u8), k as u32));
    }

    println!();
    println!("ns per value, fast path against the generic way ({VALUES} values a call, Global)");
    println!("{:>34} {:>10} {:>10}", "path", "fast", "generic");
    let row = |path: &str, fast: f64, generic: f64| println!("{path:>34} {fast:>10.3} {generic:>10.3}");
    // Reused across calls, so the extends measure the copy, not the allocation.
    let into = std::cell::RefCell::new(Vec::with_capacity_in(VALUES, Global));
    let plain_into = std::cell::RefCell::new(Vec::with_capacity_in(VALUES, Global));
    row(
        "extend_from_slice, u32",
        best(|| per_value(|| {
            let mut v = into.borrow_mut();
            v.clear();
            v.extend_from_slice(black_box(&words));
            black_box(v.as_slice());
        })),
        best(|| per_value(|| {
            let mut v = plain_into.borrow_mut();
            v.clear();
            v.extend_from_slice(black_box(&plain_words));
            black_box(v.as_slice());
        })),
    );
    row(
        "extend(&[T]), u32",
        best(|| per_value(|| {
            let mut v = into.borrow_mut();
            v.clear();
            v.extend(black_box(&words).iter());
            black_box(v.as_slice());
        })),
        best(|| per_value(|| {
            let mut v = into.borrow_mut();
            v.clear();
            v.extend(Hidden(black_box(&words).iter()));
            black_box(v.as_slice());
        })),
    );
    row(
        "insert_many at 0 of 4096, u32",
        best(|| per_value(|| {
            let mut v = into.borrow_mut();
            v.clear();
            v.insert_slice_shift(0, black_box(&words));
            black_box(v.as_slice());
        })),
        best(|| per_value(|| {
            let mut v = plain_into.borrow_mut();
            v.clear();
            v.insert_slice_shift(0, black_box(&plain_words));
            black_box(v.as_slice());
        })),
    );
    row(
        "clone_in (alloc + copy), u32",
        best(|| per_value(|| drop(black_box(black_box(&fast_vec).clone_in(Global))))),
        best(|| per_value(|| drop(black_box(black_box(&plain_vec).clone_in(Global))))),
    );
    row(
        "collect_in from map(range), u32",
        best(|| per_value(|| drop(black_box((0..black_box(VALUES) as u32).map(|i| i * 3).collect_in(Global))))),
        best(|| per_value(|| drop(black_box(Hidden((0..black_box(VALUES) as u32).map(|i| i * 3)).collect_in(Global))))),
    );
    row(
        "position of an absent byte",
        best(|| per_value(|| drop(black_box(black_box(&byte_vec).scan_for(&3))))),
        best(|| per_value(|| drop(black_box(black_box(&plain_byte_vec).scan_for(&Plain(3)))))),
    );
    row(
        "Soa<(u8, u32)> clone_in",
        best(|| per_value(|| drop(black_box(black_box(&fast_rows).clone_in(Global))))),
        best(|| per_value(|| drop(black_box(black_box(&plain_rows).clone_in(Global))))),
    );
    let ring = std::cell::RefCell::new((VecDeque::with_capacity_in(VALUES, Global), VecDeque::with_capacity_in(VALUES, Global)));
    ring.borrow_mut().0.extend(words.iter().copied());
    row(
        "VecDeque append (runs vs per value)",
        best(|| per_value(|| {
            let (a, b) = &mut *ring.borrow_mut();
            b.append(a);
            core::mem::swap(a, b);
            black_box(&*a);
        })),
        best(|| per_value(|| {
            let (a, b) = &mut *ring.borrow_mut();
            while let Some(value) = a.pop_front() {
                b.push_back(value);
            }
            core::mem::swap(a, b);
            black_box(&*a);
        })),
    );
}

/// The best of three runs of `measure`, after one to warm up.
fn best(measure: impl Fn() -> f64) -> f64 {
    measure();
    (0..3).map(|_| measure()).fold(f64::MAX, f64::min)
}

fn main() {
    // `alloc_cost shapes`: only the shapes' tables; `alloc_cost causes`: the experiments behind
    // their two shortfalls.
    match std::env::args().nth(1).as_deref() {
        Some("shapes") => return shapes(),
        Some("causes") => return causes(),
        Some("segs") => return segs(),
        Some("arc") => return arc_contention(),
        // `alloc_cost paths [root]`: the tree's shape from a real checkout, read-only.
        Some("paths") => {
            let root = std::env::args().nth(2).unwrap_or_else(|| std::string::String::from("/Volumes/Projects/ref-reth/reth"));
            return paths_cost(&root);
        }
        // One shape a process, so the run's peak memory (examplerun's `time -l`) is its own.
        Some("peak-segs") => return peak(true),
        Some("peak-vec") => return peak(false),
        _ => {}
    }
    let bump = Bump::new(64 << 20);
    let bump_erased = Erased::new(&bump);
    let global_erased = Erased::new(Global);
    let reset = || bump.reset();
    let none = || {};
    // Hidden from the optimizer: each call through these is a real indirect call.
    let bump_handle: Alloc<'_> = black_box(bump_erased.handle());
    let global_handle: Alloc<'_> = black_box(global_erased.handle());

    println!("ns per push into a growing Vec<u64> (from empty to the length)");
    println!("{:>10} {:>12} {:>12} {:>12} {:>12}", "length", "bump", "bump Alloc", "global", "global Alloc");
    for len in [16usize, 1024, 65536] {
        println!(
            "{len:>10} {:>12.2} {:>12.2} {:>12.2} {:>12.2}",
            best(|| per_push(&bump, len, &reset)),
            best(|| per_push(bump_handle, len, &reset)),
            best(|| per_push(Global, len, &none)),
            best(|| per_push(global_handle, len, &none)),
        );
    }

    println!();
    println!("ns per small allocation (a Box of the size made and dropped)");
    println!("{:>10} {:>12} {:>12} {:>12} {:>12}", "bytes", "bump", "bump Alloc", "global", "global Alloc");
    let row = |bytes: usize, measures: [f64; 4]| {
        println!("{bytes:>10} {:>12.2} {:>12.2} {:>12.2} {:>12.2}", measures[0], measures[1], measures[2], measures[3]);
    };
    row(16, [
        best(|| per_box::<_, 16>(&bump)),
        best(|| per_box::<_, 16>(bump_handle)),
        best(|| per_box::<_, 16>(Global)),
        best(|| per_box::<_, 16>(global_handle)),
    ]);
    row(128, [
        best(|| per_box::<_, 128>(&bump)),
        best(|| per_box::<_, 128>(bump_handle)),
        best(|| per_box::<_, 128>(Global)),
        best(|| per_box::<_, 128>(global_handle)),
    ]);
    row(1024, [
        best(|| per_box::<_, 1024>(&bump)),
        best(|| per_box::<_, 1024>(bump_handle)),
        best(|| per_box::<_, 1024>(Global)),
        best(|| per_box::<_, 1024>(global_handle)),
    ]);
    bump.reset();

    fast_paths();
    shapes();
}

/// The shapes in place of what std makes easy to get slow (docs/v2/plan/design/std.md).
fn shapes() {
    println!();
    println!("Lists<u32> against Vec<Vec<u32>>: rows of 0 to 8 items (lengths from a fixed LCG)");
    println!(
        "{:>9} {:>10} {:>12} {:>12} {:>14} {:>14} {:>14} {:>9}",
        "rows", "shape", "build ns/row", "scan ns/item", "heap bytes", "+16B quantum", "floor bytes", "vs floor"
    );
    for rows in [1_000usize, 1_000_000] {
        lists_row(rows);
    }
    println!();
    println!("Texts against Vec<String>: strings of 0 to 15 ASCII bytes (lengths from a fixed LCG)");
    println!(
        "{:>9} {:>12} {:>13} {:>12} {:>14} {:>14} {:>14} {:>9}",
        "strings", "shape", "push ns/each", "scan ns/each", "heap bytes", "+16B quantum", "floor bytes", "vs floor"
    );
    for count in [1_000usize, 1_000_000] {
        texts_row(count);
    }
}

/// The experiments behind the shapes' shortfalls: is `Lists`' build at 1e6 rows first touch of
/// fresh pages (build twice into the same reserved buffers; build into a frame touched before),
/// and is `Texts`' scan at 1e3 the loop or the clock (one pass of 1e3 strings is under a
/// microsecond: repeat it).
fn causes() {
    let rows = 1_000_000usize;
    let lens = lcg(rows, 9);
    let items: usize = lens.iter().sum();
    let build_into = |ours: &mut blaze_own::Lists<u32, Global>| {
        let start = Instant::now();
        for (r, &n) in lens.iter().enumerate() {
            ours.push_row((0..n as u32).map(|i| i + r as u32));
        }
        start.elapsed().as_nanos() as f64 / rows as f64
    };
    println!("Lists build at 1e6 rows, ns/row: the same reserved buffers, fresh pages then warm");
    for _ in 0..3 {
        let mut ours: blaze_own::Lists<u32, Global> = blaze_own::Lists::with_capacity_in(rows, items, Global);
        let fresh = build_into(&mut ours);
        ours.clear();
        let warm = build_into(&mut ours);
        println!("  fresh {fresh:.2}  warm {warm:.2}");
    }
    // The shape's floor in this loop: two std vectors filled by hand, reserved, so only the
    // writes and the loop are left.
    let by_hand = || {
        let start = Instant::now();
        let mut flat: std::vec::Vec<u32> = std::vec::Vec::with_capacity(items);
        let mut ends: std::vec::Vec<u32> = std::vec::Vec::with_capacity(rows);
        for (r, &n) in lens.iter().enumerate() {
            flat.extend((0..n as u32).map(|i| i + r as u32));
            ends.push(flat.len() as u32);
        }
        black_box((&flat, &ends));
        start.elapsed().as_nanos() as f64 / rows as f64
    };
    println!("two std Vecs by hand, reserved (the loop's floor), ns/row: best {:.2}", best(by_hand));
    // A frame touched before: every page of the bump buffer written once, then the build. The
    // two vectors alternate, so only the newest grows in place: room for the copies too.
    let bump = Bump::new(256 << 20);
    // SAFETY: the buffer is `bump.cap` bytes, owned by `bump`.
    unsafe { core::ptr::write_bytes(bump.base.as_ptr(), 1, bump.cap) };
    let frame = || {
        let start = Instant::now();
        let mut ours: blaze_own::Lists<u32, &Bump> = blaze_own::Lists::new_in(&bump);
        for (r, &n) in lens.iter().enumerate() {
            ours.push_row((0..n as u32).map(|i| i + r as u32));
        }
        black_box(&ours);
        drop(ours);
        bump.reset();
        start.elapsed().as_nanos() as f64 / rows as f64
    };
    println!("Lists build at 1e6 rows into a touched bump frame (growing in place), ns/row: best {:.2}", best(frame));
    let theirs = || {
        let start = Instant::now();
        let mut theirs: std::vec::Vec<std::vec::Vec<u32>> = std::vec::Vec::new();
        for (r, &n) in lens.iter().enumerate() {
            theirs.push((0..n as u32).map(|i| i + r as u32).collect());
        }
        black_box(&theirs);
        start.elapsed().as_nanos() as f64 / rows as f64
    };
    println!("Vec<Vec> build at 1e6 rows, ns/row, first in this process then best of 3: {:.2}, {:.2}", theirs(), best(theirs));

    println!();
    println!("Texts scan at 1e3 strings, ns/string, one pass against 1000 passes");
    let lens = lcg(1000, 16);
    const LETTERS: &[u8; 16] = b"abcdefghijklmnop";
    let mut ours: blaze_own::Texts<Global> = blaze_own::Texts::new_in(Global);
    let mut theirs: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    for (i, &n) in lens.iter().enumerate() {
        let start = i % 16;
        let text = core::str::from_utf8(&LETTERS[start..start + n.min(16 - start)]).unwrap_or("");
        ours.push(text);
        theirs.push(std::string::String::from(text));
    }
    let scan_ours = |passes: usize| {
        let start = Instant::now();
        let mut sum = 0usize;
        for _ in 0..passes {
            for t in black_box(&ours).iter() {
                sum = sum.wrapping_add(t.len() + usize::from(t.as_bytes().first().copied().unwrap_or(0)));
            }
        }
        black_box(sum);
        start.elapsed().as_nanos() as f64 / (passes * 1000) as f64
    };
    let scan_theirs = |passes: usize| {
        let start = Instant::now();
        let mut sum = 0usize;
        for _ in 0..passes {
            for t in black_box(&theirs) {
                sum = sum.wrapping_add(t.len() + usize::from(t.as_bytes().first().copied().unwrap_or(0)));
            }
        }
        black_box(sum);
        start.elapsed().as_nanos() as f64 / (passes * 1000) as f64
    };
    println!(
        "  Texts {:.3} / {:.3}   Vec<String> {:.3} / {:.3}",
        best(|| scan_ours(1)),
        best(|| scan_ours(1000)),
        best(|| scan_theirs(1)),
        best(|| scan_theirs(1000))
    );
}

/// `Arc` clone and drop from 1, 6, 12 and 18 threads: on one value all threads share (every
/// count change on one cache line), on a value per thread, and a borrow from a scope (no count).
/// Each thread runs `PAIRS` clone-and-drop pairs; the figure is wall time over each thread's
/// pairs, so a flat line is perfect scaling.
fn arc_contention() {
    const PAIRS: usize = 2_000_000;
    let cores = std::thread::available_parallelism().map_or(0, usize::from);
    println!("Arc clone+drop, ns per pair per thread ({PAIRS} pairs a thread; {cores} hardware threads)");
    println!("{:>8} {:>12} {:>12} {:>12} {:>9}", "threads", "one shared", "per thread", "scope borrow", "shared/own");
    for threads in [1usize, 6, 12, 18] {
        let shared = blaze_own::Arc::new_in(7u64, Global);
        let one = best(|| {
            let start = Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        for _ in 0..PAIRS {
                            drop(black_box(blaze_own::Arc::clone(black_box(&shared))));
                        }
                    });
                }
            });
            per_n(start, PAIRS)
        });
        let own = best(|| {
            let start = Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        // Aligned to its own cache line (128 bytes on Apple's cores): unpadded,
                        // the threads' small blocks shared lines and this column measured false
                        // sharing (31.6 ns at 6 threads).
                        let mine = blaze_own::Arc::new_in(Line(7), Global);
                        for _ in 0..PAIRS {
                            drop(black_box(blaze_own::Arc::clone(black_box(&mine))));
                        }
                    });
                }
            });
            per_n(start, PAIRS)
        });
        let value = 7u64;
        let borrow = best(|| {
            let start = Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        let mut sum = 0u64;
                        for _ in 0..PAIRS {
                            sum = sum.wrapping_add(*black_box(&value));
                        }
                        black_box(sum);
                    });
                }
            });
            per_n(start, PAIRS)
        });
        println!("{threads:>8} {one:>12.2} {own:>12.2} {borrow:>12.2} {:>9.1}", one / own);
    }
}

/// The `.rs` files under `root`, relative and `/`-joined, skipping `target` and dot
/// directories: a directory walk, read-only.
fn rust_files(root: &std::path::Path) -> std::vec::Vec<std::string::String> {
    let mut out = std::vec::Vec::new();
    let mut stack = std::vec![std::path::PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let entries = match std::fs::read_dir(root.join(&rel)) {
            Ok(entries) => entries,
            Err(err) => panic!("alloc_cost paths: cannot list {}: {err}", root.join(&rel).display()),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => panic!("alloc_cost paths: an entry of {}: {err}", rel.display()),
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { panic!("alloc_cost paths: the type of {name}") };
            if kind.is_dir() && !name.starts_with('.') && name != "target" {
                stack.push(rel.join(&name));
            } else if kind.is_file() && name.ends_with(".rs") {
                out.push(rel.join(&name).to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

/// `Paths` against `Vec<String>` and `Vec<PathBuf>` on reth's tree (1e3) and on 730 copies of
/// it under `copyNNN/` (1e6): bytes a path, intern, find, and comparing two paths.
fn paths_cost(root: &str) {
    use std::collections::HashSet;
    let tree = rust_files(std::path::Path::new(root));
    let mean = tree.iter().map(|p| p.len()).sum::<usize>() as f64 / tree.len() as f64;
    println!("Paths against Vec<String> and Vec<PathBuf>: the .rs files of {root} ({} files, {mean:.1} bytes a path)", tree.len());
    for copies in [1usize, 730] {
        let paths: std::vec::Vec<std::string::String> = if copies == 1 {
            tree.clone()
        } else {
            (0..copies).flat_map(|c| tree.iter().map(move |p| format!("copy{c:03}/{p}"))).collect()
        };
        let n = paths.len();
        let text_bytes: usize = paths.iter().map(|p| p.len()).sum();
        println!();
        println!("{n} paths, {text_bytes} bytes of text");

        // Intern: Paths from &str; the std forms push an owned copy (String, PathBuf) or
        // insert into a HashSet<String> (the std way to intern).
        let intern_paths = best(|| {
            let start = Instant::now();
            let mut ours: blaze_own::Paths<Global> = blaze_own::Paths::new_in(Global);
            for p in &paths {
                black_box(ours.intern_path(p));
            }
            black_box(&ours);
            per_n(start, n)
        });
        let intern_set = best(|| {
            let start = Instant::now();
            let mut set: HashSet<std::string::String> = HashSet::new();
            for p in &paths {
                if !set.contains(p.as_str()) {
                    set.insert(p.clone());
                }
            }
            black_box(&set);
            per_n(start, n)
        });
        let push_strings = best(|| {
            let start = Instant::now();
            let v: std::vec::Vec<std::string::String> = paths.iter().map(|p| std::string::String::from(p.as_str())).collect();
            black_box(&v);
            per_n(start, n)
        });
        let push_pathbufs = best(|| {
            let start = Instant::now();
            let v: std::vec::Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
            black_box(&v);
            per_n(start, n)
        });
        println!(
            "  intern ns/path: Paths {intern_paths:.1}, HashSet<String> {intern_set:.1}; push a copy: Vec<String> {push_strings:.1}, Vec<PathBuf> {push_pathbufs:.1}"
        );

        // Bytes a path.
        let mut ours: blaze_own::Paths<Global> = blaze_own::Paths::new_in(Global);
        let ids: std::vec::Vec<blaze_own::PathId> = paths.iter().map(|p| ours.intern_path(p)).collect();
        let (sealed, remap) = ours.seal_in(Global);
        let strings: std::vec::Vec<std::string::String> = paths.clone();
        let pathbufs: std::vec::Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
        let string_bytes = quantum(strings.capacity() * 24) + strings.iter().map(|s| quantum(s.capacity())).sum::<usize>();
        let pathbuf_bytes = quantum(pathbufs.capacity() * 24)
            + pathbufs.iter().map(|p| quantum(p.as_os_str().len())).sum::<usize>();
        let per = |bytes: usize| bytes as f64 / n as f64;
        println!(
            "  bytes/path: Paths {:.1} ({} dirs), sealed {:.1}, Vec<String> {:.1}, Vec<PathBuf> {:.1} (16-byte malloc quantum; text alone {:.1})",
            per(ours.heap_bytes()),
            ours.dirs(),
            per(sealed.heap_bytes()),
            per(string_bytes),
            per(pathbuf_bytes),
            per(text_bytes)
        );

        // Find: every path once, in a shuffled order.
        let order = lcg(n, n as u64);
        let view = sealed.view();
        let set: HashSet<&str> = paths.iter().map(std::string::String::as_str).collect();
        let mut sorted: std::vec::Vec<&str> = paths.iter().map(std::string::String::as_str).collect();
        sorted.sort_unstable();
        let find_paths = best(|| {
            let start = Instant::now();
            for &i in &order {
                black_box(black_box(&ours).find_path(&paths[i]));
            }
            per_n(start, n)
        });
        let find_sealed = best(|| {
            let start = Instant::now();
            for &i in &order {
                black_box(black_box(&view).find_path(&paths[i]));
            }
            per_n(start, n)
        });
        let find_set = best(|| {
            let start = Instant::now();
            for &i in &order {
                black_box(black_box(&set).contains(paths[i].as_str()));
            }
            per_n(start, n)
        });
        let find_sorted = best(|| {
            let start = Instant::now();
            for &i in &order {
                black_box(black_box(&sorted).binary_search(&paths[i].as_str()).is_ok());
            }
            per_n(start, n)
        });
        // One probe: the file's own name in a directory already known, as a caller holding a
        // `DirId` asks; set against the walk above, it says what a component costs.
        let known: std::vec::Vec<(blaze_own::DirId, &str)> =
            order.iter().map(|&i| (ours.parent(ids[i]), ours.name(ids[i]))).collect();
        let find_one = best(|| {
            let start = Instant::now();
            for &(dir, name) in &known {
                black_box(black_box(&ours).find(dir, name));
            }
            per_n(start, n)
        });
        let depth = paths.iter().map(|p| p.split('/').count()).sum::<usize>() as f64 / n as f64;
        println!(
            "  find ns/path: Paths {find_paths:.1}, sealed {find_sealed:.1}, HashSet<&str> {find_set:.1}, sorted Vec<&str> {find_sorted:.1}; one component in a known directory {find_one:.1} ({depth:.1} components a path)"
        );

        // Compare: path i against i (equal, another copy) or i + 1 (a sibling, a long common
        // prefix), alternately.
        let other_strings = strings.clone();
        let other_pathbufs = pathbufs.clone();
        let sealed_ids: std::vec::Vec<blaze_own::PathId> = ids.iter().map(|&id| remap.path(id)).collect();
        let pairs: std::vec::Vec<(usize, usize)> = order.iter().map(|&i| (i, if i % 2 == 0 { i } else { (i + 1) % n })).collect();
        let cmp_ids = best(|| {
            let start = Instant::now();
            let mut equal = 0usize;
            for &(a, b) in &pairs {
                equal += usize::from(black_box(&sealed_ids)[a] == black_box(&sealed_ids)[b]);
            }
            black_box(equal);
            per_n(start, n)
        });
        let cmp_strings = best(|| {
            let start = Instant::now();
            let mut equal = 0usize;
            for &(a, b) in &pairs {
                equal += usize::from(black_box(&strings)[a] == black_box(&other_strings)[b]);
            }
            black_box(equal);
            per_n(start, n)
        });
        let cmp_pathbufs = best(|| {
            let start = Instant::now();
            let mut equal = 0usize;
            for &(a, b) in &pairs {
                equal += usize::from(black_box(&pathbufs)[a] == black_box(&other_pathbufs)[b]);
            }
            black_box(equal);
            per_n(start, n)
        });
        println!("  compare ns/pair: PathId {cmp_ids:.2}, String {cmp_strings:.2}, PathBuf {cmp_pathbufs:.2}");
    }
}

/// A value alone on its cache line.
#[repr(align(128))]
struct Line(#[allow(dead_code, reason = "held to be counted, never read")] u64);

/// Values appended in the `Segs` rows: 1e8 `u64`s, 800 MB.
const SEGS_N: usize = 100_000_000;

/// Appends `SEGS_N` `u64`s into a `Segs` or a growing `Vec`, once, and prints the time: run
/// alone in a process for its peak memory.
fn peak(segs: bool) {
    // Mixed values: macOS compresses idle pages, and pages of counting integers compress to
    // nearly nothing, which would hide them from the resident size.
    let mix = |i: u64| i.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29);
    let start = Instant::now();
    if segs {
        let mut ours: blaze_own::Segs<u64, Global> = blaze_own::Segs::new_in(Global);
        for i in 0..SEGS_N as u64 {
            ours.push(mix(i));
        }
        println!("Segs: {SEGS_N} pushes, {:.2} ns each, {} heap bytes", per_n(start, SEGS_N), ours.heap_bytes());
        black_box(&ours);
    } else {
        let mut theirs: blaze_own::Vec<u64, Global> = blaze_own::Vec::new_in(Global);
        for i in 0..SEGS_N as u64 {
            theirs.push(mix(i));
        }
        println!("Vec: {SEGS_N} pushes, {:.2} ns each, {} heap bytes", per_n(start, SEGS_N), theirs.capacity() * 8);
        black_box(&theirs);
    }
}

fn per_n(start: Instant, n: usize) -> f64 {
    start.elapsed().as_nanos() as f64 / n as f64
}

/// `Segs` against `Vec` and a slice: append time (best of three, each in fresh memory) and
/// random reads.
fn segs() {
    println!("Segs<u64> against Vec<u64>: {SEGS_N} appends, ns each (best of 3)");
    let append_segs = best(|| {
        let start = Instant::now();
        let mut ours: blaze_own::Segs<u64, Global> = blaze_own::Segs::new_in(Global);
        for i in 0..SEGS_N as u64 {
            ours.push(i);
        }
        black_box(&ours);
        per_n(start, SEGS_N)
    });
    let append_vec = best(|| {
        let start = Instant::now();
        let mut theirs: blaze_own::Vec<u64, Global> = blaze_own::Vec::new_in(Global);
        for i in 0..SEGS_N as u64 {
            theirs.push(i);
        }
        black_box(&theirs);
        per_n(start, SEGS_N)
    });
    println!("  Segs {append_segs:.2}   Vec {append_vec:.2}");
    let mut ours: blaze_own::Segs<u64, Global> = blaze_own::Segs::new_in(Global);
    let mut flat: std::vec::Vec<u64> = std::vec::Vec::with_capacity(SEGS_N);
    for i in 0..SEGS_N as u64 {
        ours.push(i);
        flat.push(i);
    }
    let reads = 10_000_000usize;
    let at: std::vec::Vec<usize> = lcg(reads, SEGS_N as u64);
    let read_segs = best(|| {
        let start = Instant::now();
        let mut sum = 0u64;
        for &i in &at {
            sum = sum.wrapping_add(black_box(&ours)[i]);
        }
        black_box(sum);
        per_n(start, reads)
    });
    let read_slice = best(|| {
        let start = Instant::now();
        let mut sum = 0u64;
        for &i in &at {
            sum = sum.wrapping_add(black_box(flat.as_slice())[i]);
        }
        black_box(sum);
        per_n(start, reads)
    });
    // Reads that stay in cache: the index arithmetic alone.
    let hot: std::vec::Vec<usize> = lcg(reads, 4096);
    let hot_segs = best(|| {
        let start = Instant::now();
        let mut sum = 0u64;
        for &i in &hot {
            sum = sum.wrapping_add(black_box(&ours)[i]);
        }
        black_box(sum);
        per_n(start, reads)
    });
    let hot_slice = best(|| {
        let start = Instant::now();
        let mut sum = 0u64;
        for &i in &hot {
            sum = sum.wrapping_add(black_box(flat.as_slice())[i]);
        }
        black_box(sum);
        per_n(start, reads)
    });
    println!("random reads, ns each: over all 1e8 Segs {read_segs:.2} slice {read_slice:.2}; in the first 4096 Segs {hot_segs:.2} slice {hot_slice:.2}");
}

/// A fixed sequence of small numbers below `below`, the same in every shape.
fn lcg(count: usize, below: u64) -> std::vec::Vec<usize> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..count)
        .map(|_| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) % below) as usize
        })
        .collect()
}

/// The size macOS's malloc gives a block of `bytes` (its tiny zone's 16-byte quantum); 0 for none.
fn quantum(bytes: usize) -> usize {
    bytes.div_ceil(16) * 16
}

/// The best of three timed runs (after one to warm up) of `run`, which builds, measures and
/// drops one shape; the nanoseconds of its build and scan.
fn best_pair(run: impl Fn() -> (u128, u128)) -> (u128, u128) {
    run();
    (0..3).map(|_| run()).fold((u128::MAX, u128::MAX), |(a, b), (c, d)| (a.min(c), b.min(d)))
}

fn lists_row(rows: usize) {
    let lens = lcg(rows, 9);
    let items: usize = lens.iter().sum();
    let floor = items * 4 + rows * 4;
    // A scan repeated to a million rows: one pass of 1e3 is under a microsecond, near the clock.
    let passes = (1_000_000 / rows).max(1);
    let per =
        |(build, scan): (u128, u128)| (build as f64 / rows as f64, scan as f64 / (items.max(1) * passes) as f64);

    let (build, scan) = per(best_pair(|| {
        let start = Instant::now();
        let mut ours: blaze_own::Lists<u32, Global> = blaze_own::Lists::new_in(Global);
        for (r, &n) in lens.iter().enumerate() {
            ours.push_row((0..n as u32).map(|i| i + r as u32));
        }
        let build = start.elapsed().as_nanos();
        let start = Instant::now();
        let mut sum = 0u64;
        for _ in 0..passes {
            for row in black_box(&ours).iter() {
                for &v in row {
                    sum = sum.wrapping_add(u64::from(v));
                }
            }
        }
        black_box(sum);
        (build, start.elapsed().as_nanos())
    }));
    let mut ours: blaze_own::Lists<u32, Global> = blaze_own::Lists::new_in(Global);
    for (r, &n) in lens.iter().enumerate() {
        ours.push_row((0..n as u32).map(|i| i + r as u32));
    }
    let bytes = ours.heap_bytes();
    // Two blocks, each well past the tiny zone: the quantum adds nothing measurable.
    println!(
        "{rows:>9} {:>10} {build:>12.2} {scan:>12.3} {bytes:>14} {bytes:>14} {floor:>14} {:>9.2}",
        "Lists",
        bytes as f64 / floor as f64
    );
    // Sized up front (both counts known, as when rows come from a pass that counted them): no
    // growth copies, and the bytes are the floor.
    let (build, _) = per(best_pair(|| {
        let start = Instant::now();
        let mut ours: blaze_own::Lists<u32, Global> = blaze_own::Lists::with_capacity_in(rows, items, Global);
        for (r, &n) in lens.iter().enumerate() {
            ours.push_row((0..n as u32).map(|i| i + r as u32));
        }
        black_box(&ours);
        (start.elapsed().as_nanos(), 0)
    }));
    let sized: blaze_own::Lists<u32, Global> = blaze_own::Lists::with_capacity_in(rows, items, Global);
    let bytes = sized.heap_bytes();
    println!(
        "{rows:>9} {:>10} {build:>12.2} {:>12} {bytes:>14} {bytes:>14} {floor:>14} {:>9.2}",
        "Lists sized",
        "",
        bytes as f64 / floor as f64
    );

    let (build, scan) = per(best_pair(|| {
        let start = Instant::now();
        let mut theirs: std::vec::Vec<std::vec::Vec<u32>> = std::vec::Vec::new();
        for (r, &n) in lens.iter().enumerate() {
            theirs.push((0..n as u32).map(|i| i + r as u32).collect());
        }
        let build = start.elapsed().as_nanos();
        let start = Instant::now();
        let mut sum = 0u64;
        for _ in 0..passes {
            for row in black_box(&theirs) {
                for &v in row {
                    sum = sum.wrapping_add(u64::from(v));
                }
            }
        }
        black_box(sum);
        (build, start.elapsed().as_nanos())
    }));
    let mut theirs: std::vec::Vec<std::vec::Vec<u32>> = std::vec::Vec::new();
    for (r, &n) in lens.iter().enumerate() {
        theirs.push((0..n as u32).map(|i| i + r as u32).collect());
    }
    let outer = theirs.capacity() * size_of::<std::vec::Vec<u32>>();
    let bytes = outer + theirs.iter().map(|row| row.capacity() * 4).sum::<usize>();
    let rounded = quantum(outer) + theirs.iter().map(|row| quantum(row.capacity() * 4)).sum::<usize>();
    println!(
        "{rows:>9} {:>10} {build:>12.2} {scan:>12.3} {bytes:>14} {rounded:>14} {floor:>14} {:>9.2}",
        "Vec<Vec>",
        rounded as f64 / floor as f64
    );
}

fn texts_row(count: usize) {
    let lens = lcg(count, 16);
    const LETTERS: &[u8; 16] = b"abcdefghijklmnop";
    // The strings, made once: what is measured is holding them.
    let texts: std::vec::Vec<&str> = lens
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            let start = i % 16;
            let n = n.min(16 - start);
            core::str::from_utf8(&LETTERS[start..start + n]).unwrap_or("")
        })
        .collect();
    let bytes_total: usize = texts.iter().map(|t| t.len()).sum();
    let floor = bytes_total + count * 4;
    // A scan repeated to a million strings, as for `Lists`.
    let passes = (1_000_000 / count).max(1);
    let per = |(push, scan): (u128, u128)| (push as f64 / count as f64, scan as f64 / (count * passes) as f64);

    let (push, scan) = per(best_pair(|| {
        let start = Instant::now();
        let mut ours: blaze_own::Texts<Global> = blaze_own::Texts::new_in(Global);
        for t in &texts {
            ours.push(t);
        }
        let push = start.elapsed().as_nanos();
        let start = Instant::now();
        let mut sum = 0usize;
        for _ in 0..passes {
            for t in black_box(&ours).iter() {
                sum = sum.wrapping_add(t.len() + usize::from(t.as_bytes().first().copied().unwrap_or(0)));
            }
        }
        black_box(sum);
        (push, start.elapsed().as_nanos())
    }));
    let mut ours: blaze_own::Texts<Global> = blaze_own::Texts::new_in(Global);
    for t in &texts {
        ours.push(t);
    }
    let bytes = ours.heap_bytes();
    println!(
        "{count:>9} {:>12} {push:>13.2} {scan:>12.3} {bytes:>14} {bytes:>14} {floor:>14} {:>9.2}",
        "Texts",
        bytes as f64 / floor as f64
    );

    let (push, scan) = per(best_pair(|| {
        let start = Instant::now();
        let mut theirs: std::vec::Vec<std::string::String> = std::vec::Vec::new();
        for t in &texts {
            theirs.push(std::string::String::from(*t));
        }
        let push = start.elapsed().as_nanos();
        let start = Instant::now();
        let mut sum = 0usize;
        for _ in 0..passes {
            for t in black_box(&theirs) {
                sum = sum.wrapping_add(t.len() + usize::from(t.as_bytes().first().copied().unwrap_or(0)));
            }
        }
        black_box(sum);
        (push, start.elapsed().as_nanos())
    }));
    let mut theirs: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    for t in &texts {
        theirs.push(std::string::String::from(*t));
    }
    let outer = theirs.capacity() * size_of::<std::string::String>();
    let bytes = outer + theirs.iter().map(std::string::String::capacity).sum::<usize>();
    let rounded = quantum(outer) + theirs.iter().map(|t| quantum(t.capacity())).sum::<usize>();
    println!(
        "{count:>9} {:>12} {push:>13.2} {scan:>12.3} {bytes:>14} {rounded:>14} {floor:>14} {:>9.2}",
        "Vec<String>",
        rounded as f64 / floor as f64
    );

    // Lookup: the sorted, deduplicated form against a sorted Vec<String> searched the same way.
    let sorted: blaze_own::SortedTexts<Global> = blaze_own::SortedTexts::from_texts_in(&ours, Global);
    let mut model: std::vec::Vec<&str> = texts.clone();
    model.sort_unstable();
    model.dedup();
    let probes = lcg(count.min(100_000), count as u64);
    let lookup_ours = best(|| {
        let start = Instant::now();
        let mut hits = 0usize;
        for &p in &probes {
            hits += usize::from(black_box(&sorted).contains(texts[p]));
        }
        black_box(hits);
        start.elapsed().as_nanos() as f64 / probes.len() as f64
    });
    let lookup_theirs = best(|| {
        let start = Instant::now();
        let mut hits = 0usize;
        for &p in &probes {
            hits += usize::from(black_box(&model).binary_search(&texts[p]).is_ok());
        }
        black_box(hits);
        start.elapsed().as_nanos() as f64 / probes.len() as f64
    });
    println!(
        "{count:>9} {:>12} lookup ns: SortedTexts {lookup_ours:.1}, sorted Vec<&str> {lookup_theirs:.1} ({} distinct, {} bytes)",
        "",
        sorted.len(),
        sorted.heap_bytes()
    );
}
