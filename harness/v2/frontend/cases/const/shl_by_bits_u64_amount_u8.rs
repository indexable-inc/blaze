//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `1u64 << 64u8`: the amount, of another type than the left side, is compared with the left side's width
#![crate_type = "lib"]
pub const X: u64 = 1u64 << 64u8;
