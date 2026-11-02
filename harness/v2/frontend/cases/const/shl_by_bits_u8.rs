//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `1 << BITS` on u8, the amount a u32 constant equal to 8, shifts by the bit width
#![crate_type = "lib"]
pub const BITS: u32 = 8;
pub const X: u8 = 1 << BITS;
