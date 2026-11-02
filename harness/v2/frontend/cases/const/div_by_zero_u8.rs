//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `1 / 0` on u8 in a constant divides by zero
#![crate_type = "lib"]
pub const X: u8 = 1 / 0;
