use core::ptr;

use crate::lib::polyfill::cfg_select;
use crate::lib::zstd::ZSTD_customMem;

#[inline]
pub(crate) unsafe fn ZSTD_customMalloc(
    size: usize,
    customMem: ZSTD_customMem,
) -> *mut core::ffi::c_void {
    if customMem.customAlloc.is_some() ^ customMem.customFree.is_some() {
        return core::ptr::null_mut();
    }

    if let Some(f) = customMem.customAlloc {
        return f(customMem.opaque, size);
    }

    cfg_select! {
        feature = "rust-allocator" => {
            let layout = core::alloc::Layout::from_size_align_unchecked(size, 16);
            std::alloc::alloc(layout).cast()
        }
        feature = "c-allocator" => libc::malloc(size),
        _ => {
            panic!("no allocator specified");
        }
    }
}

#[inline]
pub(crate) unsafe fn ZSTD_customCalloc(
    size: usize,
    customMem: ZSTD_customMem,
) -> *mut core::ffi::c_void {
    if customMem.customAlloc.is_some() ^ customMem.customFree.is_some() {
        return core::ptr::null_mut();
    }

    if let Some(f) = customMem.customAlloc {
        let ptr = f(customMem.opaque, size);
        ptr::write_bytes(ptr, 0, size);
        return ptr;
    }

    cfg_select! {
        feature = "rust-allocator" => {
            let layout = core::alloc::Layout::from_size_align_unchecked(size, 16);
            std::alloc::alloc_zeroed(layout).cast()
        }
        feature = "c-allocator" => libc::calloc(1, size),
        _ => {
            panic!("no allocator specified");
        }
    }
}

#[inline]
pub(crate) unsafe fn ZSTD_customFree(
    ptr: *mut core::ffi::c_void,
    _size: usize,
    customMem: ZSTD_customMem,
) {
    if !ptr.is_null() {
        if let Some(f) = customMem.customFree {
            return f(customMem.opaque, ptr);
        }

        cfg_select! {
            feature = "rust-allocator" => {
                let layout = core::alloc::Layout::from_size_align_unchecked(_size, 16);
                std::alloc::dealloc(ptr.cast(), layout)
            }
            feature = "c-allocator" => {
                libc::free(ptr);
            }
            _ => {
                panic!("no allocator specified");
            }
        }
    }
}
