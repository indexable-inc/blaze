//! pure: `#[blaze::pure]` on a function that reads a `static mut`. A working layer refuses it.
//! expect: refuse

static mut COUNT: i32 = 42;

#[blaze::pure]
fn read() -> i32 {
    unsafe { COUNT }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    read()
}
