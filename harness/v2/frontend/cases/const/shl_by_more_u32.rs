//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `1u32 << 100` in a constant shifts by more than the bit width
#![crate_type = "lib"]
pub const X: u32 = 1u32 << 100;
