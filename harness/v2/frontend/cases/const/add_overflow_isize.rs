//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on isize (64-bit target) in a constant overflows
#![crate_type = "lib"]
pub const MAX: isize = 9223372036854775807;
pub const X: isize = MAX + 1;
