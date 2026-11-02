//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `A % Z` on u16, the zero read from another constant, takes the remainder by zero
#![crate_type = "lib"]
pub const A: u16 = 40000;
pub const Z: u16 = 0;
pub const X: u16 = A % Z;
