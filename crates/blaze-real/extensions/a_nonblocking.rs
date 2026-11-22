//! nonblocking: `#[blaze::nonblocking]` on a function that calls `sleep`. A working layer refuses it.
//! expect: refuse

unsafe extern "C" {
    fn sleep(seconds: u32) -> u32;
}

#[blaze::nonblocking]
fn nap() -> u32 {
    unsafe { sleep(1) }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = nap();
    0
}
