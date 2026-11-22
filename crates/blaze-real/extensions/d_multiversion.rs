//! multiversion: `#[blaze::multiversion]`, a function compiled once per target-feature level and picked at start. A working layer accepts it and builds each level.
//! expect: accept

#[blaze::multiversion]
fn sum(a: i32, b: i32) -> i32 {
    a + b
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    sum(40, 2)
}
