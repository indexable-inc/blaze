//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `5 >> 1000` on isize in a constant shifts by more than the bit width
#![crate_type = "lib"]
pub const X: isize = 5 >> 1000;
