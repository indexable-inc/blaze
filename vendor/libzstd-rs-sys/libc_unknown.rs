//! blaze: what the translation takes from libc, for wasm32-unknown-unknown, where the libc crate
//! is empty (SOURCE.md change 5). The crate root re-exports these and `extern crate self as
//! libc` makes `libc::` name them, so the translated code is unchanged.
//!
//! Each has C's signature and meaning. `malloc`, `calloc` and `free` keep a block's size in a
//! header of [`HEADER`] bytes before the pointer they hand out, since `free` is given none and
//! Rust's allocator needs the layout back; the pointer is 16-aligned, as C's `max_align_t` is.

use core::ffi::{c_int, c_void};
use std::alloc::{Layout, alloc, alloc_zeroed, dealloc};

/// C's `ptrdiff_t`: pointer-sized and signed.
pub(crate) type ptrdiff_t = isize;

/// Bytes before each block: its size, padded to the block's alignment.
const HEADER: usize = 16;
const ALIGN: usize = 16;

/// The layout of a block of `size` bytes and its header; `None` when it overflows.
fn layout(size: usize) -> Option<Layout> {
    Layout::from_size_align(size.checked_add(HEADER)?, ALIGN).ok()
}

/// # Safety
///
/// As C's `memcpy`: `dst` and `src` are valid for `n` bytes and do not overlap.
pub(crate) unsafe fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    // SAFETY: the caller's promise, as C's memcpy asks.
    unsafe { core::ptr::copy_nonoverlapping(src.cast::<u8>(), dst.cast::<u8>(), n) };
    dst
}

/// # Safety
///
/// As C's `memcmp`: `a` and `b` are valid for reads of `n` bytes.
pub(crate) unsafe fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
    if n == 0 {
        return 0;
    }
    // SAFETY: the caller's promise: both are readable for `n` bytes, and `n` is not 0, so
    // neither is null.
    let (a, b) = unsafe {
        (core::slice::from_raw_parts(a.cast::<u8>(), n), core::slice::from_raw_parts(b.cast::<u8>(), n))
    };
    match a.cmp(b) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

/// A block of `size` bytes from `get`, its size written in the header; null when it does not
/// fit, as C's `malloc` says so.
unsafe fn block(size: usize, get: unsafe fn(Layout) -> *mut u8) -> *mut c_void {
    let Some(layout) = layout(size) else { return core::ptr::null_mut() };
    // SAFETY: the layout's size is at least HEADER, never 0.
    let base = unsafe { get(layout) };
    if base.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `base` is 16-aligned and at least HEADER bytes long, so the header holds a usize
    // and the block starts within the allocation.
    unsafe {
        base.cast::<usize>().write(size);
        base.add(HEADER).cast()
    }
}

/// # Safety
///
/// As C's `malloc`; the block is freed only by [`free`].
pub(crate) unsafe fn malloc(size: usize) -> *mut c_void {
    // SAFETY: `alloc` takes the non-zero layout `block` makes.
    unsafe { block(size, alloc) }
}

/// # Safety
///
/// As C's `calloc`; the block is freed only by [`free`].
pub(crate) unsafe fn calloc(count: usize, size: usize) -> *mut c_void {
    let Some(size) = count.checked_mul(size) else { return core::ptr::null_mut() };
    // SAFETY: `alloc_zeroed` takes the non-zero layout `block` makes.
    unsafe { block(size, alloc_zeroed) }
}

/// # Safety
///
/// As C's `free`: `ptr` is null or a block [`malloc`] or [`calloc`] made, not yet freed.
pub(crate) unsafe fn free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` is HEADER bytes into a block `block` made, whose header holds the size it
    // was made with, so the layout is the one it was allocated with.
    unsafe {
        let base = ptr.cast::<u8>().sub(HEADER);
        let size = base.cast::<usize>().read();
        let layout = Layout::from_size_align_unchecked(size + HEADER, ALIGN);
        dealloc(base, layout);
    }
}
