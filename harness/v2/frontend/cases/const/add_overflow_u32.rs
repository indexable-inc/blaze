//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on u32 in a constant overflows
#![crate_type = "lib"]
pub const MAX: u32 = 4294967295;
pub const X: u32 = MAX + 1;
