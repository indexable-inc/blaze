//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on u64 in a constant overflows
#![crate_type = "lib"]
pub const MAX: u64 = 18446744073709551615;
pub const X: u64 = MAX + 1;
