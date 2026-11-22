//! verify clauses: `#![feature(blaze_verify)]`, `requires`/`ensures` as native grammar and a named return. A working layer accepts it and proves or checks the clauses.
//! expect: accept
#![feature(blaze_verify)]

fn next(x: u32) -> (r: u32)
    requires x < 10
    ensures r > x
{
    x + 1
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    next(41) as i32
}
