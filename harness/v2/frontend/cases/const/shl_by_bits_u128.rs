//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `1u128 << 128` in a constant shifts by the bit width
#![crate_type = "lib"]
pub const X: u128 = 1u128 << 128;
