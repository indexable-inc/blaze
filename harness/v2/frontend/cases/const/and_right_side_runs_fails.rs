//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `true && (1 / ZERO == 0)`: the left side is true, so the right side runs and divides by zero
#![crate_type = "lib"]
pub const ZERO: i32 = 0;
pub const X: bool = true && (1 / ZERO == 0);
