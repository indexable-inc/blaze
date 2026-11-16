// Support program s3: `extern "C"` calls (crates/blaze-build/src/cabi.rs, imports.rs). Each
// imported symbol is resolved once before `main`; arguments follow AAPCS64 (Apple's variant):
// - `strlen(p)`: a pointer in x0, a `usize` back in x0;
// - `abs(-30)`: an `i32` in w0;
// - `div(14, 2)`: a `repr(C)` struct of two `i32`s (8 bytes, C.10) returned in x0 (6.9).
// Prints `extern`, exits 5 + 30 + 7 = 42.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

#[repr(C)]
struct DivT {
    quot: i32,
    rem: i32,
}

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
    fn strlen(s: *const u8) -> usize;
    fn abs(x: i32) -> i32;
    fn div(numer: i32, denom: i32) -> DivT;
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe {
        write(1, b"extern\n" as *const u8, 7);
        let n = strlen(b"hello\0" as *const u8) as i32;
        let a = abs(-30);
        let d = div(14, 2);
        n + a + d.quot + d.rem
    }
}
