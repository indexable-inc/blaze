//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `-(-2147483648i32)`: the inner `-` is part of the literal, the outer one negates MIN and overflows
#![crate_type = "lib"]
pub const X: i32 = -(-2147483648i32);
