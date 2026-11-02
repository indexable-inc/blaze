//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `255 + 1` on u8 in a constant overflows
#![crate_type = "lib"]
pub const X: u8 = 255 + 1;
