//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// reading byte 10 of a 5-byte string constant: index out of bounds, E0080
#![crate_type = "lib"]
pub const S: &str = "hello";
pub const B: u8 = S.as_bytes()[10];
