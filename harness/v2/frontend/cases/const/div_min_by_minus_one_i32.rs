//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MIN / M1` on i32, both read from constants, overflows
#![crate_type = "lib"]
pub const MIN: i32 = -2147483648;
pub const M1: i32 = -1;
pub const X: i32 = MIN / M1;
