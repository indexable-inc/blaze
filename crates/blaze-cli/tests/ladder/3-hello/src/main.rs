// Rung 3: prints `hello` with no `core`: `write` is the C library's, declared here.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe {
        write(1, b"hello\n" as *const u8, 6);
    }
    0
}
