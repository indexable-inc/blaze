//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on i8 in a constant overflows
#![crate_type = "lib"]
pub const MAX: i8 = 127;
pub const X: i8 = MAX + 1;
