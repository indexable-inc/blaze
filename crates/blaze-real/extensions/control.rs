//! control: plain Rust, no extension. Every stage must pass it, or a pass below means nothing.
//! expect: accept

fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    add(40, 2)
}
