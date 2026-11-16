//! capabilities: `#![blaze::capabilities]` on a crate that reaches ambient IO (`write` to fd 1). A working layer refuses it.
//! expect: refuse
#![blaze::capabilities]

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe { write(1, 0 as *const u8, 0) };
    0
}
