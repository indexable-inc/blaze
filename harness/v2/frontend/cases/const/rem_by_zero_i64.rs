//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `7 % 0` on i64 in a constant takes the remainder by zero
#![crate_type = "lib"]
pub const X: i64 = 7 % 0;
