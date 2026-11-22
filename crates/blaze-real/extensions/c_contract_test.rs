//! contract_test: `#[blaze::contract_test]` makes a property test from a function's contract (`requires`/`ensures`). A working layer accepts it and generates the test.
//! expect: accept
#![feature(blaze_verify)]

#[blaze::contract_test]
fn half(x: u32) -> (r: u32)
    requires x % 2 == 0
    ensures r * 2 == x
{
    x / 2
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    half(84) as i32
}
