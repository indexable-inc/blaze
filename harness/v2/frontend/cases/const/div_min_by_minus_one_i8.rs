//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MIN / -1` on i8 in a constant overflows
#![crate_type = "lib"]
pub const MIN: i8 = -128;
pub const X: i8 = MIN / -1;
