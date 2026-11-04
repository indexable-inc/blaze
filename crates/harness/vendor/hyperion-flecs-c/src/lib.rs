//! Links the flecs static library built by build.rs, and provides the C allocator flecs and the wasm C library call.
//!
//! The allocator sits on Rust's global allocator with a 16-byte header holding the size (so `free` and `realloc` know the
//! layout). On wasm32 it is also exported as `malloc`/`calloc`/`realloc`/`free` (no C library exists there), and `abort`
//! traps. On the host the C library keeps its own malloc; flecs itself is routed to these functions through its OS API
//! (den-ecs sets the hooks before creating a world).
#![no_std]
#![allow(clippy::missing_safety_doc)]

extern crate alloc;

use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc as rs_realloc};
use core::ffi::c_void;

const HEADER: usize = 16;

/// The alignment of every block these functions return (the header's size and the layout's alignment).
pub const ALIGN: usize = HEADER;

fn layout(n: usize) -> Option<Layout> {
    Layout::from_size_align(n.checked_add(HEADER)?, HEADER).ok()
}

unsafe fn finish(base: *mut u8, n: usize) -> *mut c_void {
    if base.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `base` is a fresh block of at least HEADER bytes, 16-aligned.
    unsafe {
        base.cast::<usize>().write(n);
        base.add(HEADER).cast()
    }
}

/// `malloc` on Rust's allocator.
pub unsafe extern "C" fn den_flecs_malloc(n: usize) -> *mut c_void {
    let Some(l) = layout(n) else { return core::ptr::null_mut() };
    // SAFETY: the layout is non-zero sized (HEADER bytes at least).
    unsafe { finish(alloc(l), n) }
}

/// `calloc` on Rust's allocator.
pub unsafe extern "C" fn den_flecs_calloc(count: usize, size: usize) -> *mut c_void {
    let Some(n) = count.checked_mul(size) else { return core::ptr::null_mut() };
    let Some(l) = layout(n) else { return core::ptr::null_mut() };
    // SAFETY: as above.
    unsafe { finish(alloc_zeroed(l), n) }
}

/// `free` for blocks from these functions (null is a no-op).
pub unsafe extern "C" fn den_flecs_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    // SAFETY: `p` came from this allocator, so the header sits HEADER bytes before it and holds the size.
    unsafe {
        let base = p.cast::<u8>().sub(HEADER);
        let n = base.cast::<usize>().read();
        dealloc(base, Layout::from_size_align_unchecked(n + HEADER, HEADER));
    }
}

/// `realloc` for blocks from these functions.
pub unsafe extern "C" fn den_flecs_realloc(p: *mut c_void, n: usize) -> *mut c_void {
    if p.is_null() {
        // SAFETY: plain allocation.
        return unsafe { den_flecs_malloc(n) };
    }
    if layout(n).is_none() {
        return core::ptr::null_mut();
    }
    // SAFETY: `p` came from this allocator (see den_flecs_free); the new size fits a Layout (checked above).
    unsafe {
        let base = p.cast::<u8>().sub(HEADER);
        let old = base.cast::<usize>().read();
        let grown = rs_realloc(base, Layout::from_size_align_unchecked(old + HEADER, HEADER), n + HEADER);
        finish(grown, n)
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_libc {
    use core::ffi::c_void;

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn malloc(n: usize) -> *mut c_void {
        // SAFETY: forwards to the Rust allocator.
        unsafe { super::den_flecs_malloc(n) }
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn calloc(c: usize, n: usize) -> *mut c_void {
        // SAFETY: as above.
        unsafe { super::den_flecs_calloc(c, n) }
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn realloc(p: *mut c_void, n: usize) -> *mut c_void {
        // SAFETY: as above.
        unsafe { super::den_flecs_realloc(p, n) }
    }
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn free(p: *mut c_void) {
        // SAFETY: as above.
        unsafe { super::den_flecs_free(p) }
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn abort() -> ! {
        core::arch::wasm32::unreachable()
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn exit(_code: i32) -> ! {
        core::arch::wasm32::unreachable()
    }
}

unsafe extern "C" {
    /// `sizeof` of a flecs struct as the C compiler sees it (0 os_api, 1 type_hooks, 2 type_info, 3 component_desc,
    /// 4 entity_desc, 5 entities, 6 iter, 7 time; 0 for anything else). The Rust bindings are checked against it.
    pub fn den_flecs_layout(which: i32) -> usize;
}

/// Call once so the linker keeps the archive even when nothing else names a symbol in it.
#[inline(never)]
pub fn link() {}
