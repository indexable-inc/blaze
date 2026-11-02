//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `u8::MAX.checked_add(1).unwrap()` in a constant: the const panic of unwrapping None is E0080
#![crate_type = "lib"]
pub const X: u8 = u8::MAX.checked_add(1).unwrap();
