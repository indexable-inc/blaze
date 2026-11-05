//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `1 << 32` on i32 in a constant shifts by the type's bit width
#![crate_type = "lib"]
pub const X: i32 = 1 << 32;
