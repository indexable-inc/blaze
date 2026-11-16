//! tail calls: nightly's `become`, a guaranteed tail call. A working layer accepts it.
//! expect: accept
#![feature(explicit_tail_calls)]

fn count(n: i32, acc: i32) -> i32 {
    if n == 0 {
        return acc;
    }
    become count(n - 1, acc + 1)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    count(42, 0)
}
