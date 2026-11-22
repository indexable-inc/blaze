//! s8-ffi-01: `extern "C"` calls into the C library the program is linked with (libSystem on
//! macOS, glibc on Linux), each by its C standard or POSIX contract: `strlen`, `getpid` (> 0
//! and equal to `std::process::id()`, which is `getpid` on Unix), `abs`, `atoi` (skips leading
//! space, stops at the first non-digit), `strcmp` (< 0 when the first differing byte is
//! smaller), `malloc`/`memset`/`free`, `toupper`; then `CString`, `CStr` and C string literals
//! (`c"..."`, the Reference, "C string literal expressions"), and `NulError`'s text
//! (alloc/src/ffi/c_str.rs: "nul byte found in provided data at position: {}").

use std::ffi::{CStr, CString, c_char, c_int, c_void};

unsafe extern "C" {
    fn strlen(s: *const c_char) -> usize;
    fn getpid() -> c_int;
    fn abs(x: c_int) -> c_int;
    fn atoi(s: *const c_char) -> c_int;
    fn strcmp(a: *const c_char, b: *const c_char) -> c_int;
    fn malloc(n: usize) -> *mut c_void;
    fn free(p: *mut c_void);
    fn memset(p: *mut c_void, c: c_int, n: usize) -> *mut c_void;
    fn toupper(c: c_int) -> c_int;
}

fn main() {
    let s = CString::new("hello").unwrap();
    // SAFETY: each pointer below is a NUL-terminated string that outlives the call.
    unsafe {
        println!("{}", strlen(s.as_ptr()));
        println!("{}", strlen(c"blaze ladder".as_ptr()));
    }
    // SAFETY: `getpid` takes nothing and cannot fail.
    let pid = unsafe { getpid() };
    println!("{} {}", pid > 0, pid as u32 == std::process::id());
    // SAFETY: plain values and a NUL-terminated literal.
    unsafe {
        println!("{} {}", abs(-7), atoi(c"  -42xyz".as_ptr()));
        println!("{}", strcmp(c"abc".as_ptr(), c"abd".as_ptr()) < 0);
    }
    // SAFETY: eight bytes are allocated, seven set to 'A' and the eighth to NUL before the
    // string is read; the block is freed once, after the copy.
    let text = unsafe {
        let p = malloc(8) as *mut u8;
        assert!(!p.is_null(), "malloc(8) returned null");
        memset(p.cast(), 0x41, 7);
        *p.add(7) = 0;
        let t = CStr::from_ptr(p as *const c_char).to_str().unwrap().to_owned();
        free(p.cast());
        t
    };
    println!("{text}");
    // SAFETY: `toupper` takes any value representable as unsigned char.
    println!("{}", unsafe { toupper(b'q' as c_int) } as u8 as char);
    println!("{}", CString::new("a\0b").unwrap_err());
    println!("{:?}", c"bytes".to_bytes_with_nul());
}
