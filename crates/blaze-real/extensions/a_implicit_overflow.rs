//! explicit overflow: `#![deny(blaze::implicit_overflow)]` and a `+` the compiler cannot show fits. A working layer refuses it.
//! expect: refuse
#![deny(blaze::implicit_overflow)]

fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    add(40, 2)
}
