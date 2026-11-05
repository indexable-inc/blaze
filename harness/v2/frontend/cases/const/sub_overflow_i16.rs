//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MIN - 1` on i16 in a constant overflows
#![crate_type = "lib"]
pub const MIN: i16 = -32768;
pub const X: i16 = MIN - 1;
