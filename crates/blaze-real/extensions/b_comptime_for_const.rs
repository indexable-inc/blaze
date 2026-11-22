//! comptime `for const`: a loop unrolled at compile time over a constant range. A working layer accepts it and runs it in the constant evaluator.
//! expect: accept
#![feature(blaze_comptime)]

fn sum() -> i32 {
    let mut total = 0;
    for const i in 0..3 {
        total = total + i;
    }
    total
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    sum() + 39
}
