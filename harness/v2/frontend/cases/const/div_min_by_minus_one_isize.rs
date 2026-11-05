//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MIN / -1` on isize (64-bit target) in a constant overflows
#![crate_type = "lib"]
pub const MIN: isize = -9223372036854775808;
pub const X: isize = MIN / -1;
