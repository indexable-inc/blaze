//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on usize (64-bit target) in a constant overflows
#![crate_type = "lib"]
pub const MAX: usize = 18446744073709551615;
pub const X: usize = MAX + 1;
