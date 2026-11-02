//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `1 << -1` on i32 in a constant: a negative amount is out of range as unsigned
#![crate_type = "lib"]
pub const X: i32 = 1 << -1;
