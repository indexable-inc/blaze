//! trust: `#![blaze::trust(unsafe, reason = ..)]` on the crate: the list that admits its `unsafe`. A working layer records the grant and checks the reason is there.
//! expect: accept
#![blaze::trust(unsafe, reason = "one raw read of a pointer this function made")]

fn read(p: *const i32) -> i32 {
    unsafe { *p }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let x = 42;
    read(&x as *const i32)
}
