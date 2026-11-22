//! s8-ffi-02: C variadic calls and C calling back into Rust. `snprintf` with `%d %s %c %.2f
//! %lld` (C11 7.21.6.1: the arguments after the format take the default argument promotions,
//! so `%c` takes an `int` and `%f` a `double`; on Apple arm64 every variadic argument goes on the
//! stack, on Linux arm64 in registers: the call is built by the target's variadic rule); its
//! return is the length the full output would have had, and the buffer holds at most `n - 1`
//! bytes and a NUL. `qsort` and `bsearch` call an `extern "C" fn` defined here through a C
//! function pointer (C11 7.22.5).

use std::ffi::{CStr, c_char, c_int, c_void};

unsafe extern "C" {
    fn snprintf(buf: *mut c_char, n: usize, fmt: *const c_char, ...) -> c_int;
    fn qsort(base: *mut c_void, n: usize, size: usize, cmp: extern "C" fn(*const c_void, *const c_void) -> c_int);
    fn bsearch(
        key: *const c_void,
        base: *const c_void,
        n: usize,
        size: usize,
        cmp: extern "C" fn(*const c_void, *const c_void) -> c_int,
    ) -> *mut c_void;
}

extern "C" fn cmp_i32(a: *const c_void, b: *const c_void) -> c_int {
    // SAFETY: qsort and bsearch pass pointers to elements of the `i32` array (or the key).
    let (a, b) = unsafe { (*(a as *const i32), *(b as *const i32)) };
    a.cmp(&b) as c_int
}

fn main() {
    let mut buf = [0 as c_char; 64];
    // SAFETY: the buffer's length is passed; each argument has the type its conversion reads.
    let n = unsafe {
        snprintf(
            buf.as_mut_ptr(),
            buf.len(),
            c"%d-%s-%c-%.2f-%lld".as_ptr(),
            42 as c_int,
            c"ab".as_ptr(),
            b'z' as c_int,
            2.5f64,
            -9_000_000_000i64,
        )
    };
    // SAFETY: snprintf wrote a NUL inside the buffer.
    let out = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
    println!("{n} {out}");
    let mut v = [5i32, -3, 9, 0, 9, 1];
    // SAFETY: base, count and element size describe `v`; `cmp_i32` reads two `i32`s.
    unsafe { qsort(v.as_mut_ptr().cast(), v.len(), size_of::<i32>(), cmp_i32) };
    println!("{:?}", v);
    let key = 5i32;
    // SAFETY: as for qsort; `v` is sorted.
    let found = unsafe { bsearch((&key as *const i32).cast(), v.as_ptr().cast(), v.len(), size_of::<i32>(), cmp_i32) };
    // SAFETY: a non-null result points into `v`.
    println!("{:?}", (!found.is_null()).then(|| unsafe { (found as *const i32).offset_from(v.as_ptr()) }));
    let missing = 4i32;
    // SAFETY: as above.
    let none = unsafe { bsearch((&missing as *const i32).cast(), v.as_ptr().cast(), v.len(), size_of::<i32>(), cmp_i32) };
    println!("{}", none.is_null());
    let mut small = [0 as c_char; 8];
    // SAFETY: as above.
    let full = unsafe { snprintf(small.as_mut_ptr(), small.len(), c"%s".as_ptr(), c"truncated text".as_ptr()) };
    // SAFETY: snprintf NUL-terminates within `small`.
    println!("{} {}", full, unsafe { CStr::from_ptr(small.as_ptr()) }.to_str().unwrap());
}
