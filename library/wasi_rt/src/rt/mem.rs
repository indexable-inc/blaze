//! The memory and string functions that compiled Rust calls and std names, on compiler_builtins'
//! loops ([`crate::impls`]). The crate is `#![no_builtins]`, so these loops are never turned back
//! into calls to themselves.

use core::ffi::{c_char, c_int, c_void};

use crate::impls;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    impls::copy_forward(dest.cast(), src.cast(), n);
    dest
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let delta = (dest as usize).wrapping_sub(src as usize);
    if delta >= n {
        // dest is far enough ahead of src, or behind it (and delta wrapped).
        impls::copy_forward(dest.cast(), src.cast(), n);
    } else {
        impls::copy_backward(dest.cast(), src.cast(), n);
    }
    dest
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(s: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    impls::set_bytes(s.cast(), c as u8, n);
    s
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
    impls::compare_bytes(a.cast(), b.cast(), n)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
    impls::compare_bytes(a.cast(), b.cast(), n)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    impls::c_string_length(s)
}

/// The length of `s`, looking at no more than `max` bytes.
pub(crate) unsafe fn strnlen(s: *const c_char, max: usize) -> usize {
    let mut n = 0;
    while n < max && *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// A `malloc`ed copy of `bytes` with a NUL after it, or null (errno `ENOMEM`).
pub(crate) unsafe fn dup(bytes: &[u8]) -> *mut c_char {
    let p = crate::rt::malloc::malloc(bytes.len() + 1).cast::<u8>();
    if !p.is_null() {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        *p.add(bytes.len()) = 0;
    }
    p.cast()
}
