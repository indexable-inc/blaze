//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `64 >> S` on i8 with S an i8 constant equal to -3: a negative amount is out of range
#![crate_type = "lib"]
pub const S: i8 = -3;
pub const X: i8 = 64 >> S;
