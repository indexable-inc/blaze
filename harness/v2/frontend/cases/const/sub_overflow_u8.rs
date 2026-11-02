//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `0 - 1` on u8 in a constant overflows below MIN
#![crate_type = "lib"]
pub const X: u8 = 0 - 1;
