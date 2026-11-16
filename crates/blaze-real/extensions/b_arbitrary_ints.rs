//! arbitrary-width integers: `#![blaze::arbitrary_ints]`, `u7` as a primitive. A working layer accepts it.
//! expect: accept
#![blaze::arbitrary_ints]

fn widen(x: u7) -> i32 {
    x as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    widen(42)
}
