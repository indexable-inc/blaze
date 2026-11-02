//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `-1 >> 64` on i64 in a constant shifts by the bit width
#![crate_type = "lib"]
pub const X: i64 = -1 >> 64;
