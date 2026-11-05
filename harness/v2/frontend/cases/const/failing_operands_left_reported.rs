//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `(0 - 1) + (1 / 0)` in one constant: the left operand is evaluated first, so its overflow is the one reported
#![crate_type = "lib"]
pub const X: u32 = (0 - 1) + (1 / 0);
