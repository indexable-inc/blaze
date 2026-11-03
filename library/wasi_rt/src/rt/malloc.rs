//! `malloc` and friends on the dlmalloc crate, which takes pages with `memory.grow` (its wasm
//! backend, `core::arch::wasm64::memory_grow` on this target). std's `System` allocator calls these
//! (std/src/sys/alloc/unix.rs), as wasi-libc's dlmalloc serves them on wasm32.

use core::ffi::{c_int, c_void};

use dlmalloc::Dlmalloc;
use libc::{EINVAL, ENOMEM};

use crate::Global;

static HEAP: Global<Dlmalloc> = Global::new(Dlmalloc::new());

/// dlmalloc's own alignment, `2 * size_of::<usize>()`.
const MALLOC_ALIGN: usize = 2 * size_of::<usize>();

#[inline]
unsafe fn heap() -> &'static mut Dlmalloc {
    &mut *HEAP.get()
}

#[inline]
unsafe fn out_of_memory<T>(p: *mut T) -> *mut T {
    if p.is_null() {
        crate::rt::errno::set(ENOMEM);
    }
    p
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    out_of_memory(heap().c_malloc(size)).cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    heap().c_free(ptr.cast())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    let Some(total) = count.checked_mul(size) else {
        crate::rt::errno::set(ENOMEM);
        return core::ptr::null_mut();
    };
    out_of_memory(heap().calloc(total, MALLOC_ALIGN)).cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    out_of_memory(heap().c_realloc(ptr.cast(), size)).cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> c_int {
    if !align.is_power_of_two() || align % size_of::<*mut c_void>() != 0 {
        return EINVAL;
    }
    let p = heap().c_memalign(align, size);
    if p.is_null() {
        return ENOMEM;
    }
    *out = p.cast();
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn aligned_alloc(align: usize, size: usize) -> *mut c_void {
    if !align.is_power_of_two() {
        crate::rt::errno::set(EINVAL);
        return core::ptr::null_mut();
    }
    out_of_memory(heap().c_memalign(align, size)).cast()
}
