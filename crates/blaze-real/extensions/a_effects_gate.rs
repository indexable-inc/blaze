//! effects: `#![blaze::effects]` at the root; a function that may panic and declares nothing. A working layer refuses it.
//! expect: refuse
#![blaze::effects]

fn ratio(a: i32, b: i32) -> i32 {
    a / b
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    ratio(84, 2)
}
