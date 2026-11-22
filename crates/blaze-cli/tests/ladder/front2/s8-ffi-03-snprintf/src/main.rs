//! s8-ffi-03: one C-variadic call, `snprintf(buf, n, "%d %.1f", 42i32, 2.5f64)`, into a buffer
//! that is then written out.
//!
//! The Rust Reference, "External blocks", [items.extern.variadic.syntax]: "Functions within
//! external blocks may be made variadic by specifying ... as the last parameter." The pinned
//! library's libc declares `snprintf` so (library/vendor/libc-0.2.191/src/unix/mod.rs):
//! `pub fn snprintf(s: *mut c_char, n: size_t, format: *const c_char, ...) -> c_int;`
//!
//! The variable arguments are already C's promoted types (an `int` and a `double`): the error
//! index, E0617, "Certain Rust types must be cast before passing them to a variadic function".
//! Where they go: Apple arm64 "the caller places all variadic arguments on the stack" ("Writing
//! ARM64 code for Apple platforms"); Linux arm64 (AAPCS64) passes them as named ones, in x3
//! and v0. A call built as if not variadic puts them in x3 and v0 on Apple too, and Apple's
//! `snprintf` then reads its `va_arg`s from the stack: the line below comes out wrong.
//!
//! Expected, by C (ISO C 7.21.6.1, which `snprintf` formats by): `%d` of 42 is "42"; `%.1f` of
//! 2.5 is "2.5" (one digit after the point; 2.5 is exact in binary, so no rounding); the
//! return is the count of characters written, the NUL excluded: 6. The buffer of 16 bytes
//! holds all of it, so nothing is cut.

use std::ffi::{CStr, c_char, c_int};

unsafe extern "C" {
    fn snprintf(s: *mut c_char, n: usize, format: *const c_char, ...) -> c_int;
}

fn main() {
    let mut buf = [0 as c_char; 16];
    // SAFETY: `buf` is 16 writable bytes and `n` says so; the format is NUL-terminated and
    // names exactly the two arguments passed, an `int` and a `double`.
    let written = unsafe { snprintf(buf.as_mut_ptr(), buf.len(), c"%d %.1f".as_ptr(), 42i32, 2.5f64) };
    println!("{written}");
    // SAFETY: `snprintf` NUL-terminates within `n` bytes.
    let text = unsafe { CStr::from_ptr(buf.as_ptr()) };
    println!("{}", text.to_str().unwrap());
}
