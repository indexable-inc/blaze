//! `where const { COND }`: a blanket impl gated on a bool constant over the type (as blaze-std's `Acyclic`). A working layer accepts it.
//! expect: accept
#![feature(blaze_comptime)]

const fn small<T>() -> bool {
    true
}

trait Flat {
    fn flat(&self) -> i32 {
        42
    }
}

impl<T> Flat for T where const { small::<T>() } {}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    0u8.flat()
}
