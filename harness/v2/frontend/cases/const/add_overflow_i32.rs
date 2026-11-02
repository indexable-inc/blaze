//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on i32 in a constant overflows
#![crate_type = "lib"]
pub const MAX: i32 = 2147483647;
pub const X: i32 = MAX + 1;
