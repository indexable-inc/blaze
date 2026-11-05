//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// a constant that reads a failing constant: rustc reports the failing one, and nothing new for the reader
#![crate_type = "lib"]
pub const A: u8 = 255 + 1;
pub const B: u8 = A / 2;
