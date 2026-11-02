//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `GOOD * (A << 9)` on u8: the left side is fine, the right side's shift by 9 fails
#![crate_type = "lib"]
pub const GOOD: u8 = 3;
pub const A: u8 = 1;
pub const X: u8 = GOOD * (A << 9);
