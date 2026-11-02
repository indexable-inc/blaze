//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `100 / ZERO` on i32, the zero read from another constant, divides by zero
#![crate_type = "lib"]
pub const ZERO: i32 = 0;
pub const X: i32 = 100 / ZERO;
