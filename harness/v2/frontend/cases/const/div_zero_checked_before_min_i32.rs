//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MIN / 0` on i32: division by zero, the check that comes before `MIN / -1`
#![crate_type = "lib"]
pub const MIN: i32 = -2147483648;
pub const X: i32 = MIN / 0;
