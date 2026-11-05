//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `2^64 * 2^64` on u128 in a constant overflows
#![crate_type = "lib"]
pub const HALF: u128 = 18446744073709551616;
pub const X: u128 = HALF * HALF;
