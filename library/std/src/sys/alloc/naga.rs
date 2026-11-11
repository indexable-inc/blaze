//! blaze: the allocator of the GPU device target (`naga-unknown-unknown`,
//! docs/future/gpu/shaders.md). A shader invocation has no heap: every allocation fails, as on a
//! machine out of memory, so `handle_alloc_error` is what a reachable `Box::new` meets. blaze's
//! device backend refuses an allocation reachable from an entry point before it runs, naming it;
//! this is what std's own code is built with for the target.

use crate::alloc::Layout;
use crate::ptr;

#[inline]
pub unsafe fn alloc(_layout: Layout) -> *mut u8 {
    ptr::null_mut()
}

#[inline]
pub unsafe fn alloc_zeroed(_layout: Layout) -> *mut u8 {
    ptr::null_mut()
}

#[inline]
pub unsafe fn dealloc(_ptr: *mut u8, _layout: Layout) {
    // Nothing was allocated, so nothing is freed.
}

#[inline]
pub unsafe fn realloc(_ptr: *mut u8, _layout: Layout, _new_size: usize) -> *mut u8 {
    ptr::null_mut()
}
