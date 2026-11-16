//! `where const { false }`: the blanket impl must not hold, so the method call is an error. A working layer refuses it.
//! expect: refuse
#![feature(blaze_comptime)]

trait Flat {
    fn flat(&self) -> i32 {
        42
    }
}

impl<T> Flat for T where const { false } {}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    0u8.flat()
}
