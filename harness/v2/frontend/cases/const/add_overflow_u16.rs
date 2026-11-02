//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on u16 in a constant overflows
#![crate_type = "lib"]
pub const MAX: u16 = 65535;
pub const X: u16 = MAX + 1;
