//! What the allocator handle costs: nanoseconds per `push` into a growing vector and per small
//! allocation (a box made and dropped), through a bump allocator named directly (as a hot loop
//! names `&Frame`), through an `Alloc` handle over the same bump allocator, and through the
//! global allocator, directly and through a handle.
//!
//! `cargo run --release -p blaze-own --example alloc_cost`
//!
//! The handles pass through `black_box`, so the compiler cannot see which table they point at
//! and every call is the indirect call a function taking `Alloc<'_>` makes.

#![feature(allocator_api)]

use std::{
    alloc::{AllocError, Allocator, Global, Layout},
    cell::Cell,
    hint::black_box,
    ptr::NonNull,
    time::Instant,
};

use blaze_own::{Alloc, Box, Erased, Vec};

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
}
