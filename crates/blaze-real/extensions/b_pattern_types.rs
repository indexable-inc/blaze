//! pattern types: `#![blaze::pattern_types]`, a range in the type (`u32 is 1..`). A working layer accepts it and refuses a 0 passed in.
//! expect: accept
#![blaze::pattern_types]

fn per(total: u32, n: u32 is 1..) -> u32 {
    total / n
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    per(84, 2) as i32
}
