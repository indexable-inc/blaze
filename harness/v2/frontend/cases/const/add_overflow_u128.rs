//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on u128 in a constant overflows
#![crate_type = "lib"]
pub const MAX: u128 = 340282366920938463463374607431768211455;
pub const X: u128 = MAX + 1;
