//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// a static whose initializer `255 + 1` overflows u8: E0080 from the static's evaluation
#![crate_type = "lib"]
pub static S: u8 = 255 + 1;
