//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `256 * 256` on u16 in a constant overflows
#![crate_type = "lib"]
pub const K: u16 = 256;
pub const X: u16 = K * K;
