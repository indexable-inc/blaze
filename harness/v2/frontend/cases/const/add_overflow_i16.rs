//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on i16 in a constant overflows
#![crate_type = "lib"]
pub const MAX: i16 = 32767;
pub const X: i16 = MAX + 1;
