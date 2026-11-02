//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `0x8000 >> 16u8` on u16 in a constant shifts by the bit width
#![crate_type = "lib"]
pub const X: u16 = 0x8000 >> 16u8;
