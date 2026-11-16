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
}
